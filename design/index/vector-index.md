# Vector index

Last updated: 2026-08-22
Anchor timestamp: 2026-08-22 08:49:10 UTC +0000

## Status

**Partially Implemented.** The ownership/fencing model, Router direct-ingestion durability boundary,
and contiguous frontier implementation are present. The focused frontier PocketIC and benchmark
gates pass; unfiltered persisted canbench artifacts and the final plan gate remain pending.
Graph-only lanes that never produce a Router marker remain outside the liveness slice.
The remaining physical-layout and algorithm work is tracked below. The current stable layout uses
version 1 slab/page headers with 3-byte magic (`VSL` / `VPG`) and a binary `u8` version.

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
| Router          | vector index definitions (incl. label sets), embedding-name catalog, activation/readiness, target resolution (list-capable), fan-out + deterministic merge, maintenance policy, Router stamp allocation, immutable target assignment, and the sole durable direct-ingestion lifecycle (`mutation_id → exact Graph target + exact Vector target + canonical inputs + AwaitingGraph | AwaitingVector | AwaitingFrontier`), including pending/retry payload bytes until exact marker retirement | indexed embedding bytes outside the pending/retry intent, per-subject state, partition internals            |
| Graph           | canonical graph data (the semantic source of embeddings), vertex existence, and label membership                                                 | embedding bytes (never, not even transiently), vector-domain state |
| Vector canister | indexed embedding bytes, per-(index, subject) clock, partitions, centroids, rebuild/search                                                                         | pending/retry payload bytes still owned by Router, final graph rows, traversal, property filtering, vertex existence  |

## Embeddings are derived encodings

An embedding vector is a **derived encoding** of graph data through an external embedding model. The
graph cannot re-derive it (the model is external), so:

- the graph's canonical data is the _semantic_ source, never the _byte_ source;
- the vector canister owns indexed embedding bytes after delivery; while direct ingestion is pending
  or retryable, Router MemoryId 53 durably owns the payload bytes until exact marker retirement;
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
  remove-on-missing-row writes a deleted subject clock without creating a live row.

### Shard unregister lifecycle

Router owns the registry/readiness state, while the Vector canister owns shard subject rows and
attachment state. The definition target is assigned before any shard attach, and attach accepts
only that exact immutable target. Router claims the target in the existing durable shard row before
the first remote await; an exact replay is idempotent, while a different target cannot overwrite the
claim. When a shard is unregistered, a pending direct-ingestion row for that exact `(target, shard)`
pair blocks the operation before any lifecycle mutation. Once no such row remains, Router clears the
shard's live bit (`index_attached = false`) before the first detach await and retains the exact
Vector target/attachment identity while driving the existing generation-fenced Vector
`admin_detach_shard_canister` cursor to explicit EOF. It removes the registry row only after both the
Graph Index and Vector detach operations succeed. If either operation fails, the row and exact
Vector target/attachment identity remain available for retry; a successful unregister therefore
purges stale Vector subjects before unregister completes. Router never reuses the retired shard id.
This orchestration adds no Router
stable region, Vector MemoryId, or public endpoint.

## Mutation flow

### Ingest (stamp separation)

```text
Router ── AwaitingGraph {mutation_id, exact Graph/Vector targets, metadata, bytes} ─▶ stable outbox
Router ── VertexEmbeddingStampRequest {local_vertex_id, metadata, mutation_id} ────▶ Graph
Graph  ── accepted mutation_id / logical rejection ────────────────────────────────▶ Router
Router ── AwaitingVector {same canonical intent} ──────────────────────────────────▶ stable outbox
Router ── VectorEmbeddingSyncOp {index_id, subject, stamp, encoding, dims, metric, bytes} ─▶ Vector
Router ── AwaitingFrontier {resolved marker} ──────────────────────────────────────▶ stable outbox
Router ── safe `(Vector target, shard)` frontier ─────────────────────────────────▶ Vector
```

- The Router validates the complete public batch, revalidates the live shard, exact targets, and
  immutable definition, then synchronously allocates every nonzero stamp and persists every exact
  `AwaitingGraph` intent before the first Graph await. Any returned admission error leaves both the
  mutation counter and outbox unchanged. It then sends only metadata and the stamp to Graph. Graph
  validates vertex/tombstone state, label membership against the injected mapping, and
  payload-independent embedding metadata/encoding, then returns the stamp.
  For this direct-ingest path the stamp is validation-only: Graph writes no canonical embedding
  bytes, mutation journal row, derived-index outbox row, or watermark.
- An observed exact Graph acceptance moves only that intent to `AwaitingVector`; an observed logical
  rejection changes only that intent to `AwaitingFrontier`. Transport/decode failure or response loss
  leaves `AwaitingGraph` unchanged. Recovery replays the persisted Graph target and metadata, then
  derives the exact Vector operation from the same row. Bytes cross the subnet exactly once; the
  Graph's durable stores never carry embedding bytes.
- Two calls is the minimum: the fence requires ingest stamps and DML stamps to be comparable, and the
  graph is the only authority that sees both.

### Direct-ingestion suffix durability

The Router outbox is a bounded canonical owner of direct-ingestion work, not a client receipt table.
It stores one Candid-encoded row per nonzero `mutation_id` with exact Graph and Vector targets,
canonical metadata/subject/bytes, and an `AwaitingGraph | AwaitingVector | AwaitingFrontier` phase.
`MAX_VECTOR_INGEST_OUTBOX_ROWS` is **1024**; each row is prevalidated at the
shared `MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES` ceiling (**2 MiB**). The persisted value is
`StorableBound::Unbounded`, so the transport/admission ceiling does not determine the
`StableBTreeMap` node page size; `VectorIngestOutboxState::encode_checked` is the single
write-boundary size check. Capacity, duplicate identity, and row-encoding checks complete before
any stable row is inserted. Public `Pending` means that one of these durable phases still owns the
request; it does not claim Graph acceptance or Vector visibility.

The typed `vector_sync_batch_outcome` endpoint is the only supported batch synchronization endpoint. The Vector
`VectorSyncBatchOutcome` is an exact-prefix acknowledgement:

- `Progress { applied }` changes only the first `applied` outbox rows to `AwaitingFrontier`. Every
  later row remains `AwaitingVector`.
- `Terminal { applied, failed_index: applied, error }` changes that prefix to `AwaitingFrontier`,
  while the failed row and every later row remain `AwaitingVector` in the durable suffix.
- Transport, typed-unavailable, or malformed replies leave the entire submitted set pending. The
  Router's recovery lane retries the exact target and bytes; Vector's per-subject stamp makes a
  replay after a lost success response a no-op.

This is at-least-once retry/convergence across the Router/Vector boundary without a finite-time
convergence guarantee. The public ingest result can report `Applied` or `Pending`, but it
does not provide client-level exactly-once semantics.

`AwaitingFrontier` markers are grouped by exact `(Vector target, shard)` lane. Router derives each
marked lane's frontier from the oldest unresolved exact-lane `AwaitingGraph | AwaitingVector` intent
minus one, or from the durable mutation allocation ceiling when none remains. It dispatches only if
at least one marker is covered by that frontier. The Router calls the Router-only Vector
`admin_advance_router_frontier(shard_id, frontier)` endpoint with a bounded wait. Vector rechecks
the configured Router and exact attached shard, applies `max(current, frontier)` to MemoryId 15,
and runs one bounded subject-map GC step in the same no-await update. The cutoff remains
`min(graph_watermark, router_watermark)`. After an observed acknowledgement, Router retires only
the unchanged marker snapshot captured before the call; response loss or failure leaves markers
durable for retry. Graph-only lanes that never produce a marker remain deferred, so this slice makes
no global subject-map growth or finite-time GC claim.

`AwaitingVector` rows are scanned in groups by their persisted exact `(Vector target, shard)` lane,
with each pass able to key-scan up to the bounded 1,024-entry compact outbox while decoding at most
the selected **16 payloads per recovery tick**; one pass publishes at most one exact lane frontier.
Router `init` and `post_upgrade` re-arm the one-shot recovery timer because timers
do not survive upgrades. Appending a row calls `recovery::arm_if_needed()`; if work arrives while a
recovery pass is awaiting a remote call, the request is latched and the tick re-arms the floor delay
even when that pass found no work, preventing a lost wake. While the originating API call is still
driving a row, a heap-only guard excludes that mutation ID from the timer so two drivers cannot
resolve it concurrently. An upgrade clears the guard and durable recovery resumes the row. The scan
retains immutable targets and canonical inputs and never re-resolves a target from mutable catalog
state. Each batch measures the
complete Candid request and adaptively fits a prefix below the 2 MiB hard ceiling, targeting the
ceiling minus the fixed 500 KiB envelope headroom. The typed Vector driver processes internal chunks
of at most **32 rows**. Focused Router outbox unit coverage exercises reopen, prefix replay, malformed
outcomes, retained terminal/suffix rows, frontier marker snapshots, lost-wake re-arming, adaptive
sizing, and capacity preflight. The prior targeted PocketIC lifecycle gates each ran one exact test
and passed:
`cargo test -p gleaph-pocket-ic-tests --test adr0031_vertex_embedding_ingestion unavailable_vector_owner_rebinds_graph_and_router_direct_ingestion_outboxes -- --nocapture`
and `graph_response_loss_preserves_pregraph_intent_across_router_upgrade -- --exact --nocapture`.
They cover the earlier Graph/Vector intent path, Router/Vector upgrades, exact GQL search, and
idempotent replay. The focused frontier lifecycle gate now passes exactly one PocketIC test in 25.67s
using one `PocketIc`, one federation bootstrap, and four canister installs (Router, Property Index,
Graph, Vector). It covers response loss, Router/Vector upgrades, exact retry and marker retirement,
GC gating and physical collection, and stale-resurrection prevention. Router and Vector tests, checks,
and clippy pass. Focused canbenches measure 3.29M instructions for the single-lane 1,024-row
derivation, 9.85M for 1,024 lanes, and 2.11B for the Vector frontier plus bounded-GC step.
Unfiltered persisted canbench artifacts and the final plan gate remain pending. Neither this runtime
evidence nor the bounded Quint model is a production proof of frontier liveness, autonomous timer
firing, or a global tombstone-GC bound.

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

`embedding_name_id` remains in the operation for definition validation; `index_id` selects the
Router-owned definition. Remove ops carry empty bytes.

## Ordering fence and watermark state

### Mutation-ordering fence

Every op carries a nonzero Router-issued `mutation_id`; the vector canister keeps a per-`(index_id,
subject)` clock `current_stamp`:

- `op.stamp < current_stamp` → no-op (stale replay);
- same-stamp upsert → compare the canonical payload: an identical payload is idempotent, while a
  different payload rejects with `MutationStampConflict`;
- `op.stamp > current_stamp` → apply: upsert appends a row and advances the clock; remove marks
  deleted (row retained as a deleted clock) and advances the clock;
- remove on a missing row → write a deleted subject clock (no live row is created), so later stale
  replays remain fenced.

The Router mutation sequence is comparable with Graph DML remove stamps; the fence exists only
because delivery is async and at-least-once across the graph/vector boundary.

### Watermark state and conservative tombstone retention

The subject map retains deleted clocks for the fence. Two per-shard watermark values record progress;
the active frontier implementation makes the cutoff usable only for marked lanes:

```text
shard → { graph_watermark, router_watermark }   // piggybacked on existing calls, O(shards)
```

- `graph_watermark`: highest stamp in a contiguous applied prefix acknowledged by the attached Graph
  shard through typed `vector_sync_batch_outcome`;
- `router_watermark`: contiguous Router acknowledgement floor published through the Router-only
  attached-shard Vector endpoint; Vector stores it monotonically with `max(current, frontier)`.

The Router derives a safe frontier separately for each marked `(Vector target, shard)` lane from the
oldest unresolved exact-lane `AwaitingGraph | AwaitingVector` intent minus one, or from the durable
allocation ceiling when none remains. It dispatches only when at least one `AwaitingFrontier` marker
is covered. Vector rechecks Router ownership and the exact attached shard, then updates MemoryId 15
and runs one bounded GC step in the same no-await message. Response loss leaves the captured marker
snapshot durable; an observed acknowledgement retires only unchanged captured markers.

A deleted entry is eligible for removal only after its lane's published frontier proves
`current_stamp <= min(graph_watermark, router_watermark)` for its shard. Graph-only lanes that never
produce a marker remain deferred; therefore no global subject-map growth or finite-time GC bound is
claimed.

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

**Implemented failure boundary (2026-08-14 UTC):** `append_rows` prevalidates and plans every page,
then reserves every planned page before publishing the partition head. A reservation failure leaves
only unpublished, unreachable bytes beyond the occupied tail and never partially publishes the head.
This is covered by `append_rows_grow_failure_leaves_consistent_state` and
`append_rows_second_page_reserve_failure_is_atomic`.

### Headers (3-byte magic + u8 version; version 1, breaking)

```rust
pub struct SlabHeader {
    pub magic: [u8; 3],        // b"VSL"
    pub version: u8,           // 1
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
    pub encoding: VectorEncoding,          // implemented: F32 | I8 (F16/Bf16/U8/Binary are future)
    pub dims: u16,
    pub stride_bytes: u32,                 // stored stride, minimal per encoding
    pub pad_stride_bytes: u32,             // scoring scratch stride = align16(component_bytes × dims)
    pub aux_bytes: u32,                    // 0 | 4 | 8
    pub kernel: ScoringKernel,             // F32Dot | UpcastF32Dot
}
```

- **One index = one encoding = one dims = one stride** (different models → different indexes).
- **Scoring** uses fused kernels over the stored bytes. `I8` is **implemented** (Slice 0242): rows are
  quantized once at write with a **per-row scale** (`i8_i = clamp(round(127·x_i/s))`, `s = max|x_i|`,
  scale in row-meta aux 4) and scored with a fused i8×f32 kernel (`v_i = i8_i·scale/127`), with **no
  page-level f32 materialization**. `F16`/`Bf16`/`U8`/`Binary` are future encodings, each landing with
  its own scoring-kernel slice (the unconstructible `BinaryConvention`/popcount kernel stack was
  removed in a dead-state sweep; a `Binary` encoding returns with it).
- **Wire query contract (Model Y)**: `encoding` in the op/request/definition means the **stored/index
  encoding**; wire embedding and query bytes are **always canonical F32 (`dims*4`)**. The canister
  quantizes internally; a separate `query_encoding` field is not needed unless an I8-on-wire query
  (A2) is added later.
- **Measured recall (Slice 0244)**: on deterministic synthetic data (d=256, 512 vectors, 32 queries)
  I8 recall@10/recall@100 vs F32 exact-scan ground truth is **~0.99** for both L2-Gaussian
  (0.9906 / 0.9934) and cosine-unit-sphere (0.9906 / 0.9947). Operators should still measure recall
  on their own embedding distribution before adopting I8 (adoption is an operator decision).
- **Binary cosine (future, not implemented)**: `Bits01` → `n11·rq·rv` (`n11 = Σ popcnt(q∧v)`,
  per-row `rv = 1/√popcnt(v)`); `Signs` → `1 − 2H/d` (`H = Σ popcnt(q⊕v)`, cosine ≡ Hamming order,
  no sqrt). The convention is a per-model property; the convention/kernel plumbing ships with the
  future `Binary` encoding slice.
- **Stored precision = search precision** (accepted and documented): an `int8`/`binary` index's
  "exact rerank" is exact over the stored encoding, not full precision. Quantized indexes use
  **quantized centroids** with SPANN's representative-point replacement (k-means means of binary
  vectors are not binary).

### Aux interpretation (RowAux)

| encoding × metric            | aux_bytes | meaning                                         |
| ---------------------------- | --------- | ----------------------------------------------- |
| F32 × L2 (default)           | 0         | sub-square SIMD + early exit; no norm, no bound |
| F32 × cosine (default)       | 0         | normalized dot only                             |
| I8 (implemented, Slice 0242) | 4         | per-row quantization scale (max-abs)            |
| opt-in row-level pruning     | +4        | L2 bound `dist(v, c_p)` (sub-square path)       |
| opt-in row-level pruning     | 8         | cosine `(s_v, t_v)` or norm + bound             |

## Search algorithm

`ivf_flat` is the kind (category); SPANN is the blueprint (a high-performance instance of the same
family: inverted index + uncompressed vectors + exact rerank).

### Scoring formulation

- **L2 (default)**: `sub-square` SIMD (`Σ(q−v)²`) with HARMONY-style **dimension-blocked early exit**
  (prewarm the k-th best with the first rows, then stop each row's SIMD once its partial distance
  exceeds the running threshold; block order per-query by descending `‖q_block‖`). Monotone partial
  sums make early exit safe; no per-row norm is stored. Finiteness is **fused into the kernel**: a
  non-finite component makes the partial sum non-finite, which is skipped (returns no hit) in the same
  pass — there is no separate `row_is_finite` pre-scan on the L2 path.
- **Cosine**: indexes store **unit-normalized** rows (the vector is normalized at write; zero-norm
  ingest is rejected fail-closed), so cosine distance is `1 − dot/‖q‖` — a single SIMD `dot_f32` over
  the unit row and the raw query with `q_norm = ‖query‖` precomputed (no per-row norm computation,
  no aux field). Finiteness is fused into the dot result (a non-finite component makes the dot
  non-finite and the row is skipped) — no separate `row_is_finite` pre-scan. Cosine rebuilds train
  with **spherical k-means**: candidate rows and centroids are unit, so `L2²(cand, centroid) =
2 − 2·cos` makes the L2 assignment cosine-aware; recomputed centroid means are renormalized to unit
  (a zero-norm mean keeps the previous centroid). Because cosine centroids are unit, the nlist > 1
  cosine index uses the **same ε₂ partition scan as L2**: the query is unit-normalized so
  `L2²(q̂, c) = 2 − 2·cos` is both cosine-ordered **and** a tight, cosine-meaningful ε₂ threshold
  (the raw query's `‖q‖²` constant would otherwise make a moderate `eps` select almost every partition),
  so the partition selection bounds the scanned rows.
- The `dot + norms` formulation (FMA inner loop, precomputed `‖v‖²`) remains an opt-in alternative.
- SIMD is `f32x4` with multi-accumulator batching; the scratch is 16-byte aligned and decoded by
  direct reinterpretation (no per-row `Vec<f32>` allocation). Partition routing (`assign_partition`),
  ε₂ partition selection (`select_partitions`, over the encoded query bytes), and the rebuild
  `Training` assignment all score with the same SIMD `l2_squared_f32(bytes, centroid)` kernel (no
  `decode_f32` allocation), sharing one tie-break rule (lowest id); there is no remaining scalar
  f32×f32 L2 in the vector canister.

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
- **Why from the start**: the 8 MiB training envelope is a _per-training-job_ bound, so the
  encoding-dependent `nlist` ceiling at `d = 1536` (≈677 for F32) applies **per level**, not
  globally. A hierarchy restores trainable centroid counts and avoids the flat candidate-pool
  degeneration (pool ≈ `nlist` → k-means degrades to sampled centroids) at the design target.
  Retrofitting level semantics later would touch partition keys, centroid metadata, the rebuild state
  machine, the search path, and the definition config at once.
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
6. Filtered search (ADR 0034): the candidate allowlist from the Property Index is unchanged. For a
   **large** allowlist (≥ ~half the live rows) the vector canister scans the active rows page-batched
   and keeps rows whose subject is in the candidate set (scan-with-membership), avoiding the
   per-candidate subject-map `get`; for small allowlists it resolves each candidate via the subject map.
   Both paths yield the identical top-k (the page-scan invariant and order-independent early exit).

## Rebuild and compaction

**Rebuild is the only compaction.** The shadow-version state machine
(`Idle → Sampling → Training → Building → ReadyToPublish → Cleaning`) with dual-write and atomic
publish is retained unchanged. There is no free-span allocator, no PMA rebalance, and no routine
extent compaction: tail-only allocation grows the slab, and the Slice 9/10 tombstone-ratio policy
bounds it (`slab ≤ live/(1 − r)` for trigger ratio `r`).

Rebuild reads the vector canister's own rows (self-rebuild); the graph is never consulted.
`Sampling` freezes each candidate in its **native stored form** (row bytes + aux scale, deduped on
the pair; an `I8` row is never expanded to f32). f32 exists transiently inside centroid
seeding/training, and the trained centroids are canonical f32 end to end.

During `Building`, if the kth subject-store link fails after a partition batch append, the linked
prefix is preserved while the current row and every unlinked suffix row are tombstoned; the original
error is returned. Retrying from the preserved `Building` cursor skips subjects in the linked prefix
and rebuilds the current row and suffix, so the rebuild converges without duplicate live rows. The
regression is `rebuild_building_link_failure_tombstones_unlinked_suffix_and_retry_converges`.

## Growth model

Symbols: `N` = live subjects per index, `G` = cumulative ever-ingested, `s` = aligned stored row
stride (`align16(component_bytes × dims)`),
`m` = row metadata (4B vertex + aux 0–8B + page amortization), `r` = rebuild tombstone trigger,
`η` = B-tree overhead.

| Store                         | Growth                            | Bound / lever                                              |
| ----------------------------- | --------------------------------- | ---------------------------------------------------------- |
| `VECTOR_ROW_SLAB`             | `N/(1−r) × (m + s)`               | encoding (F32→I8 1/4, Binary 1/32); tombstone-ratio policy |
| `VECTOR_SUBJECT_TO_ID`        | `G × ~60B × η` while deleted clocks are retained | Marked lanes use `min(graph_watermark, router_watermark)` after observed frontier publication; Graph-only lanes remain deferred, so no global bound is claimed |
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

- `LogicalResource::VectorIndex(VectorIndexId)` is a provisionable resource; `VectorIndexId` is a
  u32 newtype supplied from the kernel (mirrors `IndexClusterId`). Today a graph has a single
  vector target (`VectorIndexId(0)` = whole graph); the future shard-group split assigns one id per
  contiguous graph shard group. Router builds `VectorCanisterInitArgs`
  (`router_canister` + definition/subject map seeds) for the Provision install.
- **On-demand provision (implemented):** a `CREATE VECTOR INDEX` in provisioned mode (a
  `provision_canister` is configured) on a graph with no vector target first provisions a vector
  canister via the shared admission flow (`requested_resources = [VectorIndex(0)]`), then registers
  the definition against that canister as its target. If the graph already has a vector target, the
  new definition inherits it (no re-provision). Dev mode (no `provision_canister`) registers a
  targetless `Registered` definition as before. The `CREATE VECTOR INDEX` path is async because
  provisioning is a cross-canister call.
- Router vector resolution returns a **target list** and the search orchestration is fan-out + merge
  with a **global deterministic top-k** contract (scores are comparable: same metric/dims/encoding).
- `VectorIndexOwnershipConfig` gains `index_group_size` / `group_index` (Option; `None` = whole
  graph).
- A future split requires its own ownership and routing decision.

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

At `d = 1536` a single training job is bounded by the 8 MiB envelope, which charges the sampled
pool at its native stored row width and the centroids at f32 width: an F32 job is bounded to ≈677
centroids, while an I8 job's state ceiling rises above `MAX_NLIST` and the per-iteration
distance-op budget binds first. With the level-generic structure this becomes a **per-level**
bound. A raw candidate region remains the documented fallback if per-job pools still bind.

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

## Implementation-gap traceability (non-normative)

The implementation-gap ledger is the status authority for this open
observation. This link does not amend the planned vector boundary above.

- [GAP-2026-08-20-002](../implementation-gaps.md#gap-2026-08-20-002--router-direct-vector-ingestion-durable-intent-ownership) — **In progress**: MemoryId 53 owns the three direct-ingestion phases and the Router frontier implementation; focused frontier PocketIC and benchmark gates pass, while unfiltered persisted canbench artifacts and the final plan gate remain pending. The bounded Quint model is not production proof, and Graph-only lane liveness remains deferred.

## Open items

1. **Per-level `nlist` under the 8 MiB envelope**: the level-generic structure makes the
   encoding-dependent per-training-job ceiling (≈677 for F32 at `d = 1536`; higher for I8, where the
   per-iteration op budget binds before the envelope) a per-level bound; a hierarchy restores
   trainable counts at `d = 1536`. A raw candidate region remains the documented fallback if per-job
   pools still bind.
2. **Hierarchy deployment timing** (**Slice 6 decision: ship `levels = 1` / flat, defer `levels = 2`**):
   the flat `ivf_flat` model stays the deployed form; enabling `levels = 2` (e.g. 64×64) follows the
   scan-cost measurement at the target scale. Slice 6 measured ~164K instructions per scanned row at
   `d = 1536` (`bench_ivf_d1536_*`), which makes a flat single-partition scan at the 10⁸ target
   (~148K rows/partition at `nlist ≈ 677`) an infeasible ~24B instructions — a concrete, measured
   argument for the two-level hierarchy at scale, not just a structural one.
3. **Scoring formulation** (**Slice 6 decision: sub-square + early-exit is the default; ε default
   confirmed at `0.0`**): the d=1536 ε₂ sweep isolates the cost model — ~144K ins/centroid (fixed,
   `nlist`-scaled) and ~164K ins/row (per scanned row) — so `DEFAULT_EPS_QUERY = 0.0` is the
   cost-minimal default (ε>0 strictly adds scanned-partition row cost: one→two partitions measured
   +7% at nlist256, +53% at nlist64). The recall cost of a single-partition default is documented by
   `partition_scan_eps_zero_loses_boundary_recall_that_eps_positive_recovers`; per-definition
   `eps_query` (the operator recall escape hatch) is designed but not yet wired into the definition
   config — a follow-up.
4. **Page size / slots per page**: config tuned by the scan-cost measurement.

## Related documents

- [property-index.md](property-index.md)
- [derived-state-query-semantics.md](derived-state-query-semantics.md)
- [capacity-planning.md](capacity-planning.md)
- [../architecture/overview.md](../architecture/overview.md)
- [../storage/stable-memory-inventory.md](../storage/stable-memory-inventory.md)
