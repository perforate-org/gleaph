# 0064. Vector canister redesign: ownership, layout, fencing, and ingest

Date: 2026-08-07
Status: Partially Implemented (design contract; Router direct-ingestion and catalog-driven frontier
implementation present; the focused Graph-only catalog-lane runtime gate passes; the persisted
Router artifact has all 41 terminal benchmark entries; targeted affected-crate format passes while
the workspace-wide format check is blocked by unrelated dirty paths; finite-time liveness and global
subject-map growth remain deferred)
Last revised: 2026-08-22
Anchor timestamp: 2026-08-22 17:04:19 UTC +0000

> **Summary.** The vector canister owns indexed embedding bytes; Graph holds zero embedding state.
> Router-issued nonzero `mutation_id` stamps order direct ingestion against Graph DML removes. Router
> durably owns pending/retry payload bytes before the first Graph await through the sole MemoryId 53
> lifecycle: `AwaitingGraph | AwaitingVector | AwaitingFrontier`, and retains them until exact marker
> retirement. The physical page store is owned by
> the domain-free `ic-stable-vector-page-store` crate. The active contract is
> `design/index/vector-index.md`.

## Context

Gleaph's vector search needs a durable home for embedding bytes and a search structure that is
mutable, deterministic, instruction-bounded, and capable of scaling across canisters. Three facts
determine the ownership boundary:

1. **Capacity.** At `d = 1536` (the design target), an `F32` embedding is 6,144 bytes/vertex. At
   10⁸ subjects the vector slab alone exceeds one canister's 500 GiB; a second full copy on Graph
   would duplicate those bytes and amplify every ingest.
2. **Placement mismatch.** Embedding bytes are consumed only by the vector canister, and their search
   placement is by vector-space cluster, not by graph shard. A per-shard canonical store is the wrong
   home.
3. **Research.** Industry graph+vector systems (Neo4j, Memgraph, Kuzu, TigerGraph, FalkorDB, Dgraph,
   pgvector) store embeddings as node data with label-scoped indexes and hash-stable routing; SPFresh
   (SOSP 2023) shows cluster-partitioned indexes stay mutable via boundary-only reassignment; HARMONY
   (ACM MOD 2025) shows dimension-split partitioning is communication-expensive but its early-stop
   pruning transfers.

## Decision

### 1. Vector canister owns embedding bytes; graph holds zero embedding state

The vector canister owns indexed embedding bytes plus the subject clock and search structures. The
Router's MemoryId 53 outbox owns pending/retry payload bytes until the exact `AwaitingFrontier`
marker is retired; the Router row is therefore a temporary durable retry owner, not an indexed-byte
store. The graph owns canonical graph data (the semantic source) and **no vector-domain state**: no
bytes, no registry, no per-identity clock. Recovery is self-rebuild (partition rebuild from the
canister's own rows) plus client re-ingestion for full loss. This decision relieves the
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
  Router enumerates only fully attached, non-anonymous `(Vector target, shard)` lanes from the
  canonical shard catalog and derives a frontier independently for the selected lane from the
  oldest unresolved exact-lane `AwaitingGraph | AwaitingVector` intent minus one, or from the
  durable Router allocation ceiling when no unresolved intent remains. The marker snapshot may be
  empty for a Graph-only lane. The recovery driver advances its heap-only catalog cursor before the
  await and attempts at most one lane per tick, so a failed lane yields the next lane a turn before
  retrying after the lap wraps. The Router publishes through the Router-only attached-shard Vector
  endpoint; Vector rechecks the configured Router and exact shard attachment before changing
  durable state. MemoryId 15 applies `router_watermark = max(current, frontier)`, and the same
  no-await update runs one bounded GC step. The sole GC cutoff remains
  `min(graph_watermark, router_watermark)`. Only an observed frontier acknowledgement retires
  captured marker keys; markerless publication mutates no Router stable row, and response loss or
  failure leaves marker-backed rows durable for retry. Catalog discovery makes fully attached
  Graph-only lanes eligible for best-effort progress, but no finite-time or global subject-map
  growth bound is claimed. MemoryId 2 stores one Router-private `RouterShardState` whose public
  registry projection and monotonic Vector attach epoch share the canonical row. A new attach claim
  advances the epoch before await; a concurrent same-target duplicate reuses it. Unregister advances
  the epoch while clearing both readiness bits before the first detach await and retains the exact
  Vector target for detach retry. Successful existing-row index re-registration clears that target
  while preserving the fence and restoring only `index_attached`. A new same-target attach advances
  the epoch again, so every pre-unregister finalizer exact-conflicts and only the new claim may
  publish readiness. This is the sole current MemoryId-2 value shape; fresh reinstall is required,
  with no migration, legacy/default decoder, or fallback.

  Rejected alternatives were a heap-only claim (lost on upgrade), a second stable claim table
  (another source of truth requiring co-write consistency), and adding the epoch to public
  `ShardRegistryEntry` (leaks Router orchestration identity across the canister API). The private
  same-row value keeps claim enforcement at the Router registry write owner.

### 6. Stamp-separated ingest

```text
Router ── metadata + Router-issued stamp ─▶ Graph  (validate existence/labels/encoding; return stamp)
Graph  ── VertexEmbeddingStamp {mutation_id} ────────────────────────────────────▶ Router
Router ── AwaitingGraph {mutation_id, exact targets, metadata, bytes} ─────────────▶ stable outbox
Router ── AwaitingVector {same canonical intent} ─────────────────────────────────▶ stable outbox
Router ── AwaitingFrontier {same canonical intent, marker} ───────────────────────▶ stable outbox
Router ── safe lane frontier (marker snapshot may be empty) ─────────────────────▶ Vector
```

Bytes cross the subnet **once**, never through the graph; the graph holds no bytes even transiently.
`stamp_embedding` is validation-only in this path: Router has already checked value dimension and
finiteness, while Graph checks vertex/tombstone state, required labels, and payload-independent
embedding metadata/encoding. Graph returns the Router-issued stamp without a Graph journal,
derived-index outbox, or watermark write. Before the first Graph await, Router validates the complete
batch, live shard, exact Graph/Vector targets, immutable definition, capacity, and encoded row size.
It then synchronously co-writes the final mutation-counter value and every exact `AwaitingGraph`
intent to `ROUTER_VECTOR_INGEST_OUTBOX` (MemoryId 53). An observed exact acceptance transitions only
that row to `AwaitingVector`; an observed logical rejection changes only that row to
`AwaitingFrontier`. Transport/decode failure or response loss leaves `AwaitingGraph` unchanged.
Two calls are required because the fence requires comparable ingest/DML stamps.
The graph's vector surface is `stamp_embedding` plus DML-driven removes (vertex delete, label loss),
re-derivable from graph state.

The outbox is bounded to `MAX_VECTOR_INGEST_OUTBOX_ROWS = 1024` rows. The sole fresh-install
`StableBTreeMap` at MemoryId 53 is keyed by the fixed 43-byte `VectorIngestOutboxKey`, ordered
primarily by immutable `mutation_id`, then exact `vector_target`, `shard_id`, and `phase`. Its
`VectorIngestOutboxValue` payload contains only Graph target/metadata, subject, and embedding
bytes; key-owned identity is not duplicated in the persisted value. Each encoded row is
prevalidated against `MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES` (2 MiB); capacity, duplicate
identity, and row-encoding failures are checked before the first stable insertion. A phase
transition preflights and moves the exact old key to the exact new phase key in one no-await
message, preserving capacity. The persisted value uses `StorableBound::Unbounded`, because this
transport/admission ceiling must not determine the `StableBTreeMap` node page size;
`VectorIngestOutboxState::encode_checked` remains the single write-boundary size check. Public
`Pending` means that one of the durable phases still owns the request. Fresh reinstall is required
for this layout; no migration or legacy decoder exists.

Only the typed `vector_sync_batch_outcome` endpoint is supported for batch synchronization. Its
response is an exact-prefix acknowledgement. `Progress { applied }` changes only the first `applied`
submitted rows to `AwaitingFrontier`; the unacknowledged suffix remains `AwaitingVector`.
`Terminal { applied, failed_index: applied, error }` changes that prefix to `AwaitingFrontier`,
while the failed row and every later row remain `AwaitingVector`. A transport/unavailable or
malformed response leaves every submitted row pending in its existing phase. The typed Vector
driver processes internal chunks of at most **32 rows**; the Router independently fits each complete
Candid request below the 2 MiB ceiling. This is an at-least-once retry/convergence contract without
a finite-time convergence guarantee or client-level exactly-once semantics.
Vector's per-subject stamp makes replay idempotent when a successful Vector write is followed by
response loss.

All three phases are retried by the Router's bounded recovery timer. `AwaitingGraph` uses the persisted
Graph target; `AwaitingVector` rows are grouped by the persisted exact `(Vector target, shard)` lane;
`AwaitingFrontier` rows retain the exact marker snapshot. The direct-ingestion scan may key-scan up
to the bounded 1,024-entry compact outbox while decoding at most the selected 16 payloads per tick.
Catalog discovery adds fully attached lanes whose marker snapshot is empty, advances before the
remote await, and publishes at most one exact lane per tick; failed lanes are retried only after
other catalog lanes receive their turn. Direct-ingestion replay never re-resolves its immutable
targets from mutable catalog state. Appending work, successful vector attach, and `post_upgrade`
re-arm the existing timer; its lost-wake latch remains the sole scheduler activation state. A
heap-only guard excludes rows still driven by their originating API call from timer recovery; an
upgrade clears the guard and durable recovery resumes them. Each Vector batch measures the complete
Candid request and adaptively fits a prefix below the 2 MiB hard ceiling, targeting the ceiling minus
the fixed 500 KiB envelope headroom. Focused Router unit tests cover reopen, exact-prefix
transitions, markerless exact-lane derivation, catalog enumeration, one-lane rotation, lost-wake
re-arming, immutable target retention, and adaptive sizing. The prior targeted PocketIC lifecycle
gates each ran one exact test and passed:
`cargo test -p gleaph-pocket-ic-tests --test adr0031_vertex_embedding_ingestion unavailable_vector_owner_rebinds_graph_and_router_direct_ingestion_outboxes -- --nocapture`
and `graph_response_loss_preserves_pregraph_intent_across_router_upgrade -- --exact --nocapture`.
They cover the earlier Graph/Vector intent path, Router/Vector upgrades, exact GQL search, and
idempotent replay. The focused Graph-only markerless lifecycle gate
`graph_only_markerless_lane_advances_frontier_on_autonomous_timer` passes exactly one test with
seven filtered tests, one `PocketIc`, one federation bootstrap, and four canister installs (Router,
Property Index, Graph, Vector). It proves that a fully attached catalog lane with an empty MemoryId
53 marker snapshot advances through the ordinary timer, survives an applied-but-lost response and
upgrade rediscovery, and uses the Vector `min(graph_watermark, router_watermark)` cutoff to collect
the deleted subject. The persisted Router artifact has all 41 terminal benchmark entries;
`markerless_frontier_catalog` records exactly 52,080,138 total instructions and 52,079,143
instructions in its benchmark scope. The focused non-persisted Vector
`bench_router_frontier_gc_budget` run recorded 2,106,603,774 instructions; the current persisted
artifact records 2,110,300,092 total and 2,110,299,087 scoped instructions. Eight exact Router owner unit tests and
`cargo check -p gleaph-router --tests` pass. Router all-target/all-feature clippy is blocked only by
unrelated unused imports and one needless borrow in `crates/router/src/prepared.rs`;
production-library strict clippy passes with no markerless diagnostic. The targeted affected-crate
format check passes; the workspace-wide format check is
blocked by unrelated dirty paths and never passed. The Quint refinement passes 15/15 deterministic
tests and both 5,000-sample depth-35 safe-policy runs with all requested witnesses; `quint verify`
is unrun and non-required. These runtime, benchmark, and bounded formal results do not prove
finite-time liveness or a global tombstone-GC/subject-map bound.

### 7. Physical layout (two-table pages; run table; packed payload; version 1)

Slab/page headers use a 3-byte magic (`VSL`/`VPG`) plus a binary `u8` version byte `1`.

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

### 8. EncodingRecord (multi-encoding; kernels)

`EncodingRecord { encoding, dims, stride_bytes, pad_stride_bytes, aux_bytes, kernel }` is a
first-class concept (the LARA `label → width` generalization, owned by the vector canister). One
index = one encoding = one stride. Widths derive from `(encoding, dims)`:
`stride_bytes = component_bytes × dims` and `pad_stride_bytes = align16(stride_bytes)`, the 16-byte
SIMD row alignment the page store stores as `row_stride`. Scoring materializes once per page read into
the f32 scratch. Implemented kernels: `F32Dot` (F32) and `UpcastF32Dot` (`I8`, fused i8×f32 with the
per-row scale). Aux defaults: `F32` 0 bytes (sub-square + early exit, or normalized dot), `I8` 4 bytes
(scale, mandatory); row-level pruning is an opt-in `+4/+8` bytes. A `Binary` encoding (bit-packed rows,
popcount scoring) is future work and ships its own convention/kernel slice — the unconstructible
`BinaryConvention`/`BinaryPopcnt` stack was removed in a dead-state sweep rather than kept unreachable.

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

### 10. Compaction = rebuild; plus an opt-in bounded slab reclaim

Routine compaction stays rebuild-only: no free-span allocator, no PMA rebalance, no automatic
policy trigger. Tail-only allocation grows the slab; the Slice 9/10 tombstone-ratio policy bounds
it (`slab <= live/(1 − r)`). In addition, plan 0278 adds an **opt-in, Router-driven bounded slab
compaction** for drained dead space: a durable driver state machine copies live pages down into a
dense prefix — bytes persisted before each page's single `VectorPageMeta.slab_offset` swap — then
rewinds `occupied_tail` once at finalize. The kernel stays append-only; there is still no
free-span reuse and no row-level defragmentation inside partially-live pages.

### 11. Multi-canister readiness

Single canister today; two future axes are distinguished. **A. Shard-group split** (near-term future;
mirrors ADR 0019; industry standard): per-canister IVF, Router fan-out + merge, rebuild stays
canister-local; the run table's shard sharing becomes most effective. **B. Partition-based split**
(future ADR; SPANN §7): global centroids, partition ownership, query-aware dispatch; requires global
training, cross-canister rebuild, and a routing story (SPFresh/LIRE boundary-only reassignment makes
the subject→canister routing index mostly static). Prepared now: Router resolution returns a target
list with a global deterministic top-k merge contract; `VectorIndexOwnershipConfig` gains
`index_group_size`/`group_index` (Option). Any future split needs a separate ADR.

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
- Indexed Vector bytes plus temporary Router pending/retry payload bytes; self-rebuild; no graph
  backfill path.
- Deterministic, mutable ordering via `mutation_id`; catalog-attached lanes, including Graph-only
  lanes with an empty marker snapshot, can advance the existing Router watermark and bounded GC
  cutoff, while no finite-time or global subject-map growth bound is claimed.
- Label-scoped definitions match industry practice and give cheap graph-side presence determination.
- Fresh layout is a clean break (version 1) with fail-closed rejection of old data.
- Multi-encoding (f16/int8/binary) gives the storage levers needed at `d = 1536`.
- Physical layer is unit-testable and canbench-able in isolation.

### Negative / costs

- Recovery on total vector-canister loss requires client re-ingestion.
- Per-training-job `nlist` bound at `d = 1536` under the 8 MiB envelope, which charges the sampled
  pool at native stored-row width and the centroids at f32 width (≈677 per F32 job; an I8 job's
  state ceiling rises above `MAX_NLIST`, where the per-iteration op budget binds first); the
  level-generic structure makes it a per-level bound, restoring trainable counts (raw candidate
  region remains the documented fallback).
- Label loss removes embeddings (IC deviation from the industry "data outlives labels" model).
- Crate split adds a workspace/CI dependency edge; the boundary API must be designed carefully.

## Alternatives considered

| Alternative                                                           | Why rejected                                                                                                                                       |
| --------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| Keep the graph canonical byte store                                   | Capacity (2nd full copy in the fastest-growing canister), relay amplification, placement mismatch                                                  |
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
   implemented; the attached Graph advances `graph_watermark`, and the Router frontier endpoint
   monotonically advances `router_watermark` for an exact attached shard while running one bounded
   GC step with cutoff `min(graph_watermark, router_watermark)`. Focused markerless PocketIC and
   benchmark evidence pass; the persisted Router artifact has all 41 terminal benchmark entries,
including `markerless_frontier_catalog` total 52,080,138 and scope 52,079,143 instructions. The
   targeted affected-crate format check passes; the workspace-wide format check is blocked by
   unrelated dirty paths and never passed. The physical layout and search/encoding roadmap remain
   planned; graph embedding regions are removed.
4. **[partial]** Rework the graph: `stamp_embedding`, label-diff removes, remove embedding regions;
   the direct-ingest stamp is validation-only and does not update the existing DML mutation
   consumers.
5. **[partial]** Rework the Router: label sets, target-list resolution, global merge contract,
   immutable definition/shard target assignment, and the three direct-ingestion intent phases are
   implemented. Frontier derivation is per exact catalog-attached target/shard lane, scans stable
   keys without decoding payloads, and may publish an empty marker snapshot; marker-backed lanes
   retire only an exact marker-key snapshot after observed acknowledgement. `AwaitingFrontier` has
   no legal payload mutation: phase or lane changes create a different key. The recovery driver
   advances one catalog lane per tick with fair retry rotation. Focused markerless PocketIC and
   benchmark evidence pass; the persisted Router artifact has all 41 terminal benchmark entries,
including `markerless_frontier_catalog` total 52,080,138 and scope 52,079,143 instructions. The
   targeted affected-crate format check passes; the workspace-wide format check is blocked by
   unrelated dirty paths and never passed. A future boundary split requires a separate ADR.
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
