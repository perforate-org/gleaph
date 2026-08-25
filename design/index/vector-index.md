# Vector index

Last updated: 2026-08-22
Anchor timestamp: 2026-08-22 17:56:38 UTC +0000

## Status

**Partially Implemented.** The ownership/fencing model, Router direct-ingestion durability boundary,
and catalog-driven frontier implementation are present. The focused Graph-only markerless PocketIC
gate passes exactly one test with seven filtered tests, one `PocketIc`, one federation bootstrap,
and four canister installs. The persisted Router `crates/router/canbench_results.yml` artifact has
all 41 Router benchmark entries terminal; `markerless_frontier_catalog` records exactly 52,080,138
total instructions and 52,079,143 instructions in its benchmark scope. A focused non-persisted
Vector `bench_router_frontier_gc_budget` run and persisted artifact formed the dated Plan 0277
baseline at 2,110,300,092 total and 2,110,299,087 scoped instructions. The later live artifact
currently contains 2,084,327,695 total and 2,084,326,690 scoped instructions from unrelated work and is not
Plan 0277 evidence.
The targeted affected-crate format check passes; the workspace-wide format check is blocked by
unrelated dirty paths and never passed. Fully attached catalog lanes, including Graph-only lanes
with an empty marker snapshot, are eligible for best-effort recovery; finite-time liveness and a
global subject-map growth bound remain deferred.
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
3. **Simplest structure that meets the budget.** No PMA, no free-span reuse, no maintained
   reverse maps: append-only positions on every hot path. Routine compaction stays
   rebuild-as-compaction; an **opt-in, Router-driven bounded maintenance driver** (plan 0278) may
   additionally relocate whole live pages into a dense prefix through each page's single
   `VectorPageMeta.slab_offset` indirection and rewind `occupied_tail` once at finalize.
4. **Original tier defines quality (revised by ADR 0079).** The stored encoding (`F32` | `I8`) is
   the data and the only source of exact scores, idempotency comparison, and rebuild
   carry-forward; a generation may additionally carry a compressed code tier whose sketches
   accelerate only the first-stage scan — every emitted hit is exactly rescored over original
   bytes from the same loaded page, and advertised result quality stays the original tier.
   Non-`F32` indexes remain approximate by contract, documented per definition.

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
only that exact immutable target. Router MemoryId 2 stores one private `RouterShardState` containing
the public registry projection and a monotonic `vector_attach_epoch`. A new target claim advances
the epoch before the first remote await; a concurrent same-target duplicate reuses the exact claim,
while a different target cannot overwrite it. When a shard is unregistered, a pending
direct-ingestion row for that exact `(target, shard)`
pair blocks the operation before any lifecycle mutation. Once no such row remains, Router clears the
shard's index and Vector readiness bits (`index_attached = false`,
`vector_index_attached = false`) and advances the attach epoch atomically before the first detach
await. The resulting exact `{ vector_target, epoch }` unregister claim is revalidated with both
readiness bits still false before each outbound detach and atomically before final row removal. It
retains the exact Vector target as detach-retry identity while driving the existing
generation-fenced Vector
`admin_detach_shard_canister` cursor to explicit EOF. It removes the registry row only after both the
Graph Index and Vector detach operations succeed. If either operation fails, the row and exact
Vector retry target remain available. If that retained row is index re-registered, the successful
post-attach Router commit advances the epoch once and co-writes `index_attached = true`,
`vector_canister = None`, and `vector_index_attached = false`; an already completed concurrent
re-registration is a no-op. A new explicit attach advances the epoch again even when it reclaims the
same target principal. Every pre-unregister finalizer and unregister continuation then fails the
exact epoch check and cannot publish readiness, detach newer ownership, or remove the new row. Only
the new attach claim's finalizer may restore readiness. The Graph Index and Vector owners retain
their independent generation-fenced detach sessions, so a detach issued for old ownership cannot
cross a later remote reattach. A successful unregister therefore purges stale Vector subjects
before unregister completes. Router never reuses the retired shard id.
This orchestration adds no Router
stable region, Vector MemoryId, or public endpoint. MemoryId 2 uses only this current
`RouterShardState` value shape; fresh reinstall is required and there is no migration, legacy
decoder, serde default, or fallback.

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

`AwaitingFrontier` markers are grouped by exact `(Vector target, shard)` lane, while the canonical
Router shard catalog discovers fully attached non-anonymous lanes even when no direct-ingestion
marker exists. For each selected lane, Router derives the frontier from the oldest unresolved
exact-lane `AwaitingGraph | AwaitingVector` intent minus one, or from the durable mutation
allocation ceiling when none remains. Unresolved direct intents in another target or shard do not
lower the selected lane, and a marker snapshot may be empty. The Router calls the Router-only Vector
`admin_advance_router_frontier(shard_id, frontier)` endpoint with a bounded wait. Vector rechecks
the configured Router and exact attached shard, applies `max(current, frontier)` to MemoryId 15,
and runs one bounded subject-map GC step in the same no-await update. The cutoff remains
`min(graph_watermark, router_watermark)`. After an observed acknowledgement, Router retires only
the unchanged marker snapshot captured before the call; markerless publication retires nothing, and
response loss or failure leaves marker-backed rows durable for retry. One catalog lane is attempted
per recovery tick; the cursor advances before await so a failed lane cannot monopolize ticks and is
retried after other lanes receive their turn. Catalog-attached Graph-only lanes are eligible for
best-effort progress, but this slice makes no global subject-map growth or finite-time GC claim.

`AwaitingVector` rows are scanned in groups by their persisted exact `(Vector target, shard)` lane,
with each pass able to key-scan up to the bounded 1,024-entry compact outbox while decoding at most
the selected **16 payloads per recovery tick**. Catalog discovery scans a bounded canonical shard
catalog page and publishes at most one exact lane frontier per tick, including an empty marker
snapshot. The cursor advances before await, then wraps after a complete lap; a failure leaves only
that lane retryable and gives later lanes their turn. Router `init` and `post_upgrade` re-arm the
one-shot recovery timer because timers do not survive upgrades. Appending a row or completing a
vector attach calls `recovery::arm_if_needed()`; if work arrives while a recovery pass is awaiting a
remote call, the request is latched and the tick re-arms the floor delay even when that pass found no
work, preventing a lost wake. While the originating API call is still driving a row, a heap-only
guard excludes that mutation ID from the timer so two drivers cannot resolve it concurrently. An
upgrade clears the guard and durable recovery resumes the row. Direct-ingestion replay retains
immutable targets and canonical inputs; catalog state is used only to discover fully attached
markerless lanes. Each batch measures the complete Candid request and adaptively fits a prefix below
the 2 MiB hard ceiling, targeting the ceiling minus the fixed 500 KiB envelope headroom. The typed
Vector driver processes internal chunks of at most **32 rows**. Focused Router outbox unit coverage
exercises reopen, prefix replay, malformed outcomes, retained terminal/suffix rows, exact marker and
markerless frontier snapshots, catalog rotation, lost-wake re-arming, adaptive sizing, and capacity
preflight. The prior targeted PocketIC lifecycle gates each ran one exact test and passed:
`cargo test -p gleaph-pocket-ic-tests --test adr0031_vertex_embedding_ingestion unavailable_vector_owner_rebinds_graph_and_router_direct_ingestion_outboxes -- --nocapture`
and `graph_response_loss_preserves_pregraph_intent_across_router_upgrade -- --exact --nocapture`.
They cover the earlier Graph/Vector intent path, Router/Vector upgrades, exact GQL search, and
idempotent replay. The focused markerless lifecycle gate
`graph_only_markerless_lane_advances_frontier_on_autonomous_timer` passes exactly one test with
seven filtered tests, one `PocketIc`, one federation bootstrap, and four canister installs (Router,
Property Index, Graph, Vector). It exercises an empty MemoryId 53 lane, the ordinary timer, an
applied-but-lost frontier response, upgrade rediscovery, and physical deletion after Vector applies
the `min(graph_watermark, router_watermark)` cutoff. The persisted Router artifact has all 41
terminal benchmark entries; `markerless_frontier_catalog` records exactly 52,080,138 total
instructions and 52,079,143 instructions in its benchmark scope. The dated Plan 0277
focused/persisted Vector frontier-plus-GC baseline records 2,110,300,092 total and 2,110,299,087
scoped instructions. The later live artifact currently records 2,084,327,695 total and
2,084,326,690 scoped instructions from unrelated work and is not Plan 0277 evidence. Nine exact Router owner unit tests
and `cargo check -p gleaph-router --tests` pass. Router all-target/all-feature clippy is blocked only
by unrelated unused imports and one needless borrow in `crates/router/src/prepared.rs`;
production-library strict clippy passes with no markerless diagnostic. The Quint refinement passes
15/15 deterministic tests and both 5,000-sample depth-35
safe-policy runs with all requested witnesses; `quint verify` is unrun and non-required. The
targeted affected-crate format check passes, while the workspace-wide format check is blocked by
unrelated dirty paths and never passed. These runtime, benchmark, and bounded formal results do not
prove finite-time frontier liveness or a global tombstone-GC/subject-map bound.

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
the active frontier implementation makes the cutoff usable for every fully attached catalog lane,
including markerless Graph-owned lanes:

```text
shard → { graph_watermark, router_watermark }   // piggybacked on existing calls, O(shards)
```

- `graph_watermark`: highest stamp in a contiguous applied prefix acknowledged by the attached Graph
  shard through typed `vector_sync_batch_outcome`;
- `router_watermark`: contiguous Router acknowledgement floor published through the Router-only
  attached-shard Vector endpoint; Vector stores it monotonically with `max(current, frontier)`.

The Router derives a safe frontier separately for each fully attached catalog `(Vector target, shard)`
lane from the oldest unresolved exact-lane `AwaitingGraph | AwaitingVector` intent minus one, or
from the durable allocation ceiling when none remains. It dispatches one lane per recovery tick even
when the exact marker snapshot is empty. Vector rechecks Router ownership and the exact attached
shard, then updates MemoryId 15 monotonically and runs one bounded GC step in the same no-await
message. Response loss leaves captured marker rows durable; an observed acknowledgement retires only
unchanged captured markers, while a markerless acknowledgement retires no Router row.

A deleted entry is eligible for removal only after its lane's published frontier proves
`current_stamp <= min(graph_watermark, router_watermark)` for its shard. Fully attached Graph-only
lanes can now publish this frontier through catalog discovery, but no global subject-map growth or
finite-time GC bound is claimed.

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
    pub code_stride: u32,      // Slice 6 (ADR 0079): per-row code segment width; 0 = tier off
}
```

Reopen validation is fail-closed: magic/version must match, `meta_stride ∈ {4,8,12}`, and the page
span must be overflow-safe. `code_stride` widens the header to 32 bytes (offset `24..28`; magic and
version unchanged — reinstall required after the layout replacement), must be `0` or a multiple of 8,
and makes the header **self-describing** so mixed generations coexist: strict header-definition
agreement applies only to active-generation pages (`PageKey.index_version ==
def.active_index_version`); teardown-resident old/shadow pages get self-consistency checks only, and
compaction spans derive from each page's own header rather than the definition.

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
| `VECTOR_REBUILD_POOL`       | grown to the bounded pool geometry on rebuild start (≤ 128 pages) |

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
- **Stored precision = search precision (revised by ADR 0079, Slice 6, 2026-08-24)**: the
  original tier (`F32` | `I8`) defines the advertised result quality and stays the only source
  of exact scores, idempotency comparison, and rebuild carry-forward. A generation may
  additionally carry a **code tier** — a compressed first-stage sketch used to accelerate the
  scan while every emitted hit is exactly rescored over the original bytes (see
  [Two-tier precision code tier](#two-tier-precision-code-tier-implemented-in-slice-6-2026-08-24)).
  An `int8`/`binary` index's "exact rerank" remains exact over the stored encoding; quantized
  indexes use quantized centroids with SPANN's representative-point replacement.

### Two-tier precision code tier (**implemented in Slice 6, 2026-08-24**; ADR 0079)

A rebuild started with `admin_start_vector_rebuild(code_tier = Some(true))` publishes a
generation whose rows carry, behind the original bytes on the same page, a per-row code segment:

```text
[code_aux 8B = ‖x‖² f32 | φ_x f32][codes ceil(P/64)·8B],   P = next_pow2(dims)
```

- **Sketch**: sign bits of the randomized Walsh–Hadamard rotation of the row — zero-pad `dims`
  to `P`, apply seeded per-coordinate sign flips (splitmix64 stream keyed by the def-frozen
  `rotation_seed`, one pure integer mix per coordinate), unnormalized WHT butterfly, then a
  single `1/√P` scaling. `O(P log P)` per row at write; the identical rotation + binarization is
  applied once per query.
- **First-stage estimator** (per row: one XNOR+popcount word pass plus a few flops):
  `dist² ≈ ‖q‖² + ‖x‖² − 2‖q‖‖x‖·clamp(s/(φ_q·φ_x), −1, 1)` with `s = (2·pc − P)/P`,
  `φ = ⟨sign(x̃)/√P, x̂⟩` stored in code aux. The bit-op kernel
  (`popcount_xnor_words`) lives in the physical page-store crate beside the other scoring
  kernels; the estimator math stays in the vector canister (`code_tier.rs`).
- **Search path**: Stage A keeps the top-`C` shortlist per loaded page
  (`C = clamp(8k, 128..=1024)`, estimate ties resolve to the lowest subject); Stage B rescors
  shortlist rows exactly from the same scratch (zero stable re-reads) into the unchanged bounded
  top-k. Rows skip Stage B only when their provable Cauchy–Schwarz lower bound already exceeds
  the current k-th best exact distance — the bound never changes an emitted result. Output is
  the exact top-k over the shortlist union; recall beyond the envelope is measured in tests and
  benches rather than guaranteed (1.00 @k10 measured on clustered d4/d96 fixtures).
- **Layout & lifecycle**: `PageHeader` grew a self-describing `code_stride` (28 → 32 B;
  VSL/VPG magic and format version unchanged — reinstall required); reopen strictness applies
  to active-generation pages only, teardown-resident pages are self-consistency checked, and
  compaction spans derive from page headers so mixed geometries coexist. The def schema grew to
  59 B under `GLEAPH-VECDEF-03` (`code_tier` + frozen `code_stride_bytes` +
  `rotation_seed`). Shadow geometry derives from one pure function (`shape_def_for`) shared by
  dual-write upserts, the Building batch, and the publish flip; the rebuild-pool header (format
  version 3) carries the shadow `code_tier` flag from start to `Building`. Filtered allowlist
  scans stay exact; flat/two-level hierarchies both accept the tier.

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

### Hierarchical partition structure (**implemented in Slice 5, 2026-08-24**; flat remains the default)

The partition structure is level-generic and **implemented**: `VectorIndexDef` carries `levels: u8`
and `nlist_fine: u32` (fixed-width 46-byte record, schema `GLEAPH-VECDEF-02`; the flat default is
`levels = 1, nlist_fine = 1`, so the leaf count is the invariant `nlist × nlist_fine`). Centroid
keys carry an explicit **level tag**: `PartitionKey::coarse(index, version, c)` names the level-0
coarse centroid `c`, and leaf keys pack their path as `partition_id = coarse_id × nlist_fine +
fine_id` (17-byte key, schema `GLEAPH-PARTKEY-1`). Heads and pages exist **for leaves only**; a
two-level generation stores its coarse set inside `IVF_CENTROIDS` under the coarse-tagged keys.

- **Flat is the degenerate case (`levels = 1`)** — one level of `nlist` centroids, no fine training,
  single-stage selection. Every flat code path, wire shape, and byte layout is unchanged by the
  hierarchy; `levels = 2` activates only when a rebuild starts with
  `admin_start_vector_rebuild(fine_nlist = Some(f))`, `f >= 2`.
- **Training (`TrainCoarse` → `TrainFine`)**: a two-level rebuild first runs k-means-lite over the
  whole pool at the coarse count (identical convergence rule and one-iteration-per-step budget as
  flat `Training`), then one bounded per-subtree job per coarse id in ascending order (each also one
  k-means-lite iteration per step over that subtree's members). Subtree membership is persisted as a
  per-row coarse-id array in the rebuild-pool region (layout version 2) so resume is exact.
  **Empty/insufficient subtree rules** (deterministic, lowest-id tie-breaks): a coarse with zero
  members replicates its coarse centroid into every fine slot; a subtree with `0 < m < f` members
  runs k-means on those `m` seeds and fills each unfilled slot (ascending) with a copy of the
  trained centroid nearest to the subtree's member mean.
- **Search iterates levels**: score the query against the cached level-0 coarse set, ε₂-select
  subtrees, read each selected subtree's contiguous child range `[c·f, (c+1)·f)` straight from
  stable memory (fine sets are **never cached** — an extension of the active-version cache scope),
  ε₂-select leaves within it with the same rule, then scan the selected leaves in full. The same
  single `eps_query` value applies at both stages; per-level eps is a deferred follow-up. The
  untrained/exact-scan fallback walks all leaves.
- **Bounds stay per level**: `MAX_NLIST` applies to the coarse count and to each subtree's fan-in,
  and the total leaf count is capped by `MAX_LEAVES = 65,536` at start time (u32 partition-id
  packing plus per-generation head capacity). The rebuild-pool region charges both levels at the
  shared work-centroid count `max(coarse, f)`.
- **Deployment stays measured**: `levels = 1` (flat behavior) remains the shipped default and the
  Router policy passes `fine_nlist = None`; enabling `levels = 2` (e.g., 64×64) follows the
  scan-cost measurement at the target scale.

### Search path

1. Heap centroid cache (query path is read-only; a miss reads stable for that call only). A
   two-level generation caches its **level-0 coarse set only**; fine child sets are always
   stable-read per use.
2. Query × centroids of the active generation's scoring set (SIMD; `nlist <= MAX_NLIST` per level):
   the leaf set for flat, the coarse set for `levels = 2`.
3. ε₂ partition selection. Two-level (`levels = 2`): ε₂-select coarse subtrees, then read each
   selected subtree's contiguous leaf range `[c·f, (c+1)·f)` and ε₂-select within it with the same
   single `eps_query`, returning globally ascending leaf ids; an unreadable child range widens to
   the full subtree (fail-open) and the untrained fallback walks all leaves.
4. Per selected partition: read extents as contiguous spans → tombstone skip (bit 31) → positional
   validation against the subject map → (opt-in) row-level bound pruning → SIMD. When the scanned
   generation carries the code tier (ADR 0079), this step instead runs Stage A (per-row XNOR+popcount
   estimate, per-page top-`C` shortlist) and Stage B (exact same-scratch rerank with lower-bound
   pruning); centroid selection and filtered allowlist scans are unchanged.
5. Deterministic top-k heap ordered by `(score, subject)`.
6. Filtered search (ADR 0034): the candidate allowlist from the Property Index is unchanged. For a
   **large** allowlist (≥ ~half the live rows) the vector canister scans the active rows page-batched
   and keeps rows whose subject is in the candidate set (scan-with-membership), avoiding the
   per-candidate subject-map `get`; for small allowlists it resolves each candidate via the subject map.
   Both paths yield the identical top-k (the page-scan invariant and order-independent early exit).

## Rebuild and compaction

**Rebuild is the routine compaction.** The shadow-version state machine
(`Idle → Sampling → Training → Building → ReadyToPublish → Cleaning`) with dual-write and atomic
publish is retained unchanged. There is no free-span allocator, no PMA rebalance, and no automatic
extent-compaction trigger: tail-only allocation grows the slab, and the Slice 9/10 tombstone-ratio
policy bounds it (`slab ≤ live/(1 − r)` for trigger ratio `r`).

**Opt-in bounded slab compaction (plan 0278)** reclaims the dead bytes that rebuild cleanup and
tombstone-GC page teardowns leave behind. A Router-driven driver (`admin_start_vector_slab_compact`
→ repeated `admin_vector_slab_compact_step` → `admin_vector_slab_compact_status`) works entirely
above the append-only kernel:

- `VectorPageMeta.slab_offset` is the **only** reference to a page's bytes (subject `SlotRef`s are
  page-relative; centroids live in their own regions), so moving one page = copy its bytes +
  swap one fixed-16 B directory value.
- Start freezes the snapshot source range `[slab header end, occupied_tail at start)` in the
  durable `VECTOR_SLAB_COMPACTION_STATE` record (MemoryId 11) together with the write cursor and
  lap cursor, so an interrupted or upgraded canister resumes fail-closed.
- Each step examines a bounded slice of the page directory and copies at most a byte budget down
  into the dense prefix, in ascending original-offset order so no copy can overwrite a not-yet-
  read source; each page's bytes are persisted strictly before its meta swap, so an interrupted
  move leaves duplicate dead bytes below — never a dangling offset.
- Pages appended after start land above the snapshot range and are never touched; metas dropped
  mid-compaction by GC/cleanup are skipped by the read cursor.
- When a full directory lap finds nothing live inside the range, finalize fails closed if any live
  span sits inside the reclaimed gap, then persists `occupied_tail = max(write_cursor, highest
  live span end)` exactly once: a quiescent store reclaims to header-only, while post-start
  appends keep the tail above the gap.

There is still no free-span reuse and no row-level defragmentation inside partially-live pages;
automatic policy triggers for this driver are deferred with the maintenance trust-model work.

Rebuild reads the vector canister's own rows (self-rebuild); the graph is never consulted.
`Sampling` freezes each candidate in its **native stored form** (row bytes + aux scale, deduped on
the pair; an `I8` row is never expanded to f32). f32 exists transiently inside centroid
seeding/training, and the trained centroids are canonical f32 end to end.

The frozen candidate pool and the Training centroid work area live in a dedicated **single-tenant
raw region** (`VECTOR_REBUILD_POOL`, MemoryId 18; ADR 0033 implementation), not in the lifecycle
record: `[96 B header][capacity × (pad-stride row + aux)][nlist × f32 centroids]` under a magic +
version + binding + lengths header. The durable `VectorRebuildStateRecord` is scalars-only
(`pool_len`, cursors, counters), so no step decodes or re-encodes pool bytes through Candid —
Sampling appends rows to the region, dedup streams the durable rows through a transient hash map
with exact byte verification (no whole-pool clone), and Training reads rows individually. Every
step validates the region header fail-closed against the definition geometry and the durable
scalars before mutating (`RebuildPoolInvalid`). Admission is bounded by the physical region budget
(8 MiB) plus the per-iteration distance-op budget; the former Candid-envelope constraint is gone.
Because the region holds one pool, a second index's `admin_start_vector_rebuild` is rejected with
`RebuildAlreadyActive` while another index binds it — cross-index rebuilds serialize at admission,
per-index semantics unchanged. The binding is released when the lifecycle leaves
`Sampling`/`Training` for good: abort entry, teardown completion to `Idle`, the `Failed`
transition, and the coordinated definition-domain reset. This is a fresh layout cutover (format
version 1, breaking): reinstall required, no migration or compatibility reader.

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
| `VECTOR_ROW_SLAB` (code tier, ADR 0079) | `+ N × c` per tier-on generation | `c = 8 + ceil(P/64)·8` with `P = next_pow2(dims)` (d1536 → P=2048 → **264 B/row**, +4.3% vs F32; d768 → 136 B); page capacity shrinks under the fixed byte budget (`slots_per_page` rederived by `shape_def_for`) |
| `VECTOR_SUBJECT_TO_ID`        | `G × ~60B × η` while deleted clocks are retained | Every fully attached catalog lane uses `min(graph_watermark, router_watermark)` after frontier publication, including markerless Graph-only lanes; no finite-time or global bound is claimed |
| `VECTOR_ROW_SLAB`             | `N/(1−r) × (m + s)`               | encoding (F32→I8 1/4, Binary 1/32); tombstone-ratio policy; opt-in bounded slab compaction reclaims drained spans (plan 0278) |
| `VECTOR_PAGE_META` / heads    | O(N / slots per page), O(nlist)   | negligible                                                 |
| `VECTOR_REBUILD_POOL`         | fixed ≤ 8 MiB while a rebuild is in flight | single-tenant region budget + per-iteration op budget cap the pool; released at teardown (ADR 0033 implementation) |
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
| 5     | two-level hierarchy (e.g., 64×64)                                                  | **implemented (Slice 5, 2026-08-24)**: `levels = 2` trains via `TrainCoarse` → `TrainFine`, searches in two ε₂ stages, and activates only on an explicit `fine_nlist` rebuild; flat (`levels = 1`) stays the deployed default |
| —     | two-tier precision code tier (RaBitQ first stage + same-page exact rerank)         | **implemented (Slice 6, 2026-08-24; ADR 0079)**: opt-in per generation via `admin_start_vector_rebuild(code_tier = Some(true))`; flat and two-level both accept it; tier off stays the default |
| 6     | closure replication (coarse-level, opt-in)                                         | Slice 11+ candidate                                                                                             |
| —     | `ivf_pq`, global HNSW                                                              | **deferred** (PQ recall; HNSW: random reads, unbounded instruction budget, delete repair, cross-canister edges) |

At `d = 1536` a single training job is bounded by the 8 MiB rebuild-pool region, which charges the
sampled pool at its native stored row width and the centroids at f32 width (plus a per-row coarse-id
array for two-level rebuilds): an F32 job is bounded to ≈677 centroids, while an I8 job's state
ceiling rises above `MAX_NLIST` and the per-iteration distance-op budget binds first. With the
two-level structure the work-area budgets are evaluated at `max(coarse, f)`, so both levels share
one bound. A raw candidate region remains the documented fallback if per-job pools still bind.

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

The implementation-gap ledger records the resolved status of this capability. This link does not
amend the planned vector boundary above.

- [GAP-2026-08-20-002](../implementation-gaps.md#gap-2026-08-20-002--router-direct-vector-ingestion-durable-intent-ownership) — **Resolved (2026-08-22)**: MemoryId 53 owns the three direct-ingestion phases and the Router frontier implementation; the canonical shard catalog now discovers fully attached markerless lanes and the one-lane recovery driver preserves exact-lane safety. The named Graph-only PocketIC gate passes one test with seven filtered, and the persisted Router artifact has all 41 terminal benchmark entries, with `markerless_frontier_catalog` at exactly 52,080,138 total and 52,079,143 scoped instructions. The dated Plan 0277 focused/persisted Vector frontier-plus-GC baseline records 2,110,300,092 total and 2,110,299,087 scoped instructions. The later live artifact currently records 2,084,327,695 total and 2,084,326,690 scoped instructions from unrelated work and is not Plan 0277 evidence. Nine exact Router owner tests and Router check pass; all-target/all-feature clippy is blocked only by unrelated `prepared.rs` unused-import and needless-borrow diagnostics, while production-library strict clippy passes. The targeted affected-crate format check passes, while the workspace-wide format check is blocked by unrelated dirty paths and never passed. The bounded Quint model is not production proof; finite-time liveness and global subject-map growth remain deferred.

## Open items

1. **Per-level `nlist` under the pool-region budget**: the two-level structure makes the
   encoding-dependent per-training-job ceiling (≈677 for F32 at `d = 1536`; higher for I8, where the
   per-iteration op budget binds before the region) a per-level bound; a hierarchy restores
   trainable counts at `d = 1536`. A raw candidate region remains the documented fallback if per-job
   pools still bind.
2. **Hierarchy deployment timing** (**Slice 5 implemented `levels = 2` mechanics; flat remains the deployed default**):
   the flat `ivf_flat` model stays the shipped form — a two-level rebuild starts only with an
   explicit `fine_nlist` and the Router policy passes `None`. Enabling `levels = 2` (e.g. 64×64) at
   scale follows the scan-cost measurement at the target scale, plus the deferred per-level
   `eps_query`. Slice 6 measured ~164K instructions per scanned row at
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
