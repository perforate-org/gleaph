# Vector index

Last updated: 2026-06-30
Anchor timestamp: 2026-06-30 16:03:13 UTC +0000

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

Slice 6 is implemented: the first real `ivf_flat` **partition-page read path**. It adds a derived
`VECTOR_ID_TO_SUBJECT` (MemoryId 11) reverse locator and a `partition_page_scan` that scores the
query against the index's centroids, selects the `nprobe` nearest partitions, and scans only those
partitions' page chains. Each scanned `PageRow` is reverse-mapped (`VECTOR_ID_TO_SUBJECT`) back to
its subject and re-validated against `VECTOR_SUBJECT_TO_ID` (not deleted, `vector_id` matches, `slot`
points at exactly this page/slot/generation) before scoring, so `VECTOR_SUBJECT_TO_ID` stays the
single freshness source of truth and the reverse map is only a locator. `vector_search` selects the
read path: it uses the **exact subject-map scan** (Slice 5) when `nlist <= 1` or the centroids are
not ready/current, and the partition-page scan otherwise; a stale or incomplete centroid set falls
back to the exact scan with no error. `nprobe` is the only recall knob — the selected partitions are
scanned **in full**, so the result is the exact top-k over those partitions and there is no mid-scan
budget that could silently truncate it (`VectorSearchResult` carries no partial/cursor marker). The
in-canister default is `nprobe = min(4, nlist)` (clamped to `1..=nlist`); the public Router/kernel
request stays algorithm-neutral (no `nprobe` on the wire), and an internal `vector_search_tuned`
varies `nprobe` for tests/benchmarks only.

Slice 7 is implemented: a production, vector-canister-owned **bounded shadow-version rebuild** that
turns a degenerate (`nlist = 1`) — or an already-partitioned — index into an `nlist > 1` index, and
makes published `nlist > 1` indexes **mutable** (removing the Slice 6 "seeded fixtures are immutable"
limitation). The `SubjectMapEntry` gains a second `shadow_slot`, a new `VECTOR_REBUILD_STATE` region
(MemoryId 12) holds the per-index lifecycle, and the registry is now 13 regions (MemoryId 0–12). See
*Slice 7 production rebuild + dual-write (implemented)* below. Centroid layouts are still also
producible by the test/bench `seed_ivf_for_test` helper, but production no longer depends on it.

Slice 8 is implemented: a bounded, deterministic **k-means-lite `Training` phase** between `Sampling`
and `Building` that refines the centroids (Slice 7 used the first `nlist` distinct samples verbatim),
plus a head-only **partition-health** summary for skew visibility. No new stable region. See *Slice 8
training quality + partition health (implemented)* below.

[ADR 0032](../adr/0032-vector-index-slab-page-store.md) replaces the `VECTOR_PAGE` large-value page
store with a vector-index-owned composite slab page store: a `VECTOR_PAGE_META` directory (MemoryId
10) of per-page `{ slab_offset, capacity, row_count, live_count, row_stride, tombstone_count }` plus a
raw `VECTOR_ROW_SLAB` region (MemoryId 13) holding structure-of-arrays row bytes
(`vector_id`/`generation`/`subject_locator`/`tombstone_bits`/`vector_bytes`) behind a magic/version
header. The two regions form one composite store (`PAGE_STORE`) that opens together and fails closed
on a partial layout, with a valid empty-initialized store treated as a normal reopen. The development
page representation has no deployed runtime state, so this is a fresh layout cutover with no old-page
migration, compatibility reader, or canonical backfill/rebuild step. Append is fallible (slab `grow`
can fail) and write-then-commit ordered; row tombstoning owns the page-meta and `PartitionHead`
`live_len` accounting; page cleanup deletes `VECTOR_PAGE_META` entries only (no slab tail rewind in
this slice — dropped bytes are leaked dead space). ADR 0032 preserves the ADR 0031 freshness
contract: `VECTOR_SUBJECT_TO_ID` remains the live-clock/source-of-truth row, while the row-local
`subject_locator` is derived scan acceleration that retires `VECTOR_ID_TO_SUBJECT` from the
partition-scan hot path (the region is retained; the search path re-validates each candidate against
`VECTOR_SUBJECT_TO_ID.current_slot_for(active)`).

The slab store exposes a derived, admin-only observability query (`admin_vector_slab_stats`,
router-guarded) over the ADR 0032 page store. It is maintenance observation, not search truth: it is
computed purely from `VECTOR_PAGE_META` plus the slab header, never reads row bytes or
`VECTOR_SUBJECT_TO_ID`, and never feeds search/mutation/rebuild/freshness decisions —
`VECTOR_SUBJECT_TO_ID` remains the freshness source of truth. Because `VECTOR_ROW_SLAB` is a single
global allocation domain, the physical slab facts (`slab_size_bytes`, `occupied_tail_bytes`,
`referenced_page_bytes_global`, `estimated_unreferenced_bytes`) are always whole-slab global, while
an optional `index_id` scopes only the logical counters and the per-version breakdown. The reported
`physical_live_row_count` is `VectorPageMeta.live_count` (physical non-tombstone rows), which can
exceed the searchable count because the search freshness check skips stale/meta-drift rows; the
dead-space estimate is approximate and intentionally conservative (it grows as cleanup deletes page
meta without rewinding the slab tail). `admin_vector_slab_stats` is an unbounded full page-meta scan
retained as a convenience query; for large stores the IC-safe path is `admin_vector_slab_stats_step`,
a cursor/budgeted variant that scans at most `max_pages` page-meta entries per call (clamped
server-side) and returns an opaque `PageKey` cursor to resume from. Its per-step results are partial
and additive: the caller repeats until `exhausted` and sums them, except the physical snapshots
(`slab_size_bytes`/`occupied_tail_bytes`) which are repeated (take any step's value). Each step still
scans the whole map for global referenced bytes even under an `index_id` filter, so each step's
`estimated_unreferenced_bytes` is reported as `0`; the caller recomputes it once after merging as
`occupied_tail_bytes - slab_header_len - sum(referenced_page_bytes_global)`. The step cursor is
external caller input, so a malformed cursor is rejected with an error rather than trapping. The
stepped path is a bounded best-effort scan, **not** a point-in-time snapshot: the cursor is only a
`PageKey`, so it has no snapshot isolation against concurrent `VECTOR_PAGE_META` writes between calls
(a page inserted before the cursor is missed, a counted-then-deleted page lingers in the merge, and
the last step's `occupied_tail_bytes` may pair with earlier-state referenced bytes). For an exact
whole-slab figure, run the steps during a quiescent window or use the single-call
`admin_vector_slab_stats`; the diagnostic-only counters tolerate small cross-call drift otherwise.
Neither query changes allocation, and any compaction/tail-rewind work remains deferred.

Slice 9 is implemented: bounded page-meta **tombstone-ratio / total-row partition health**
(`admin_vector_partition_health_step`), a policy-driven **rebuild recommendation + trigger**
(`recommend_partition_maintenance` / `admin_start_vector_rebuild_if_recommended`, no autonomous
timer), and a transient **heap centroid cache** (`admin_vector_centroid_cache_warmup` / `_clear` /
`_status`, read-only on the `#[query]` search path). No new stable region. See *Slice 9 maintenance
visibility + centroid cache (implemented)* below.

Slice 10 is implemented: **Router-forwarded maintenance** for the full Slice 7-9 surface, a
Router-owned **maintenance policy catalog** (`ROUTER_VECTOR_MAINTENANCE_POLICIES`, MemoryId 44,
disabled by default), and a **vector-owned maintenance execution state** (`VECTOR_MAINTENANCE_STATE`,
MemoryId 14) driven one bounded unit per Router push (`admin_vector_maintenance_step`). Publish stays
explicit (the step stops at `ReadyToPublish`). See *Slice 10 maintenance orchestration (implemented)*
below.

Still deferred to Slice 11+: full/balanced k-means and k-means++ init, autonomous (timer-driven)
partition tombstone-cleanup scheduling, candidate pagination, query ranking/merge, PQ/HNSW, and
`VectorSubject::Edge`. The standard vector-index
kind is `ivf_flat`; `flat` is collapsed into degenerate `ivf_flat` rather than a separate kind, and
later `ivf_pq` or experimental `hnsw` implementations must preserve the same canonical/derived
boundary.

### Slice 7 production rebuild + dual-write (implemented)

A bounded shadow-version rebuild trains centroids and builds partition pages for a new
`index_version`, then **atomically publishes** it, without losing canonical mutations that arrive
while the build is in flight. The concurrency model is **dual-write to both the active and the shadow
`index_version` while a build is active** (quiesce and delta-replay were rejected: quiesce is
operationally costly and delta-replay reintroduces the watermark design dual-write avoids). The
rebuild is driven by Router-guarded admin endpoints and every long-running phase is bounded and
cursor-resumable so no single message performs an O(N) sweep:

- `admin_start_vector_rebuild(index_id, nlist, sample_limit)` — O(1); validates params
  (`2 <= nlist <= MAX_NLIST`, `nlist <= sample_limit <= MAX_REBUILD_SAMPLE_LIMIT`, and the Slice 8
  feasibility check `2 * nlist * stride_bytes + MAX_REBUILD_STATE_OVERHEAD_BYTES <=
  MAX_REBUILD_STATE_BYTES` (combined candidate-pool + centroid durable state fits the encoded
  envelope) **and** `nlist^2 * dims <= MAX_REBUILD_TRAINING_DISTANCE_OPS` (one training iteration over
  `>= nlist` candidates fits the per-message op budget); `F32`/`L2Squared`, `Idle`) and enters
  `Sampling` without scanning subjects or writing centroids.
- `admin_vector_rebuild_step(index_id, max_subjects)` — drives `Sampling` (collect a **bounded
  distinct candidate pool**, capped by the smaller of the combined-state byte budget and the
  distance-op count, then transition to `Training` if `>= nlist` distinct candidates were collected,
  else `Failed`), then `Training` (one deterministic k-means-lite iteration per step; see Slice 8
  below) which writes the refined `IVF_CENTROIDS`, then `Building` (shadow every live subject's vector
  into its nearest target partition), reaching `ReadyToPublish`. Each `Sampling`/`Building` step is
  bounded on **both** axes: the caller's `max_subjects` is clamped to `1..=MAX_REBUILD_STEP_WORK` (row
  count) and the step also breaks once the transient vector bytes it buffers reach
  `MAX_REBUILD_STEP_VECTOR_BYTES` (heap bytes, since `stride_bytes` scales with `dims`), always
  buffering at least one vector so it makes forward progress. Insufficient distinct live vectors
  (range or `sample_limit` exhausted with `< nlist` distinct) ends in `Failed` (nothing persisted,
  O(1) recoverable to `Idle` via abort).
- `admin_publish_vector_rebuild(index_id)` — **O(1)**: completeness is an invariant established by
  `Building` + dual-write, so publish performs no live-subject scan. It flips
  `def.active_index_version` + `nlist` and the centroid metadata in one message, then enters
  `Cleaning`.
- `admin_vector_rebuild_cleanup_step(index_id, max_work)` — drives both the post-publish `Cleaning`
  teardown (collapse `shadow_slot -> slot`, repoint the reverse locator, drop the old version's
  pages/heads/centroids) and the `Aborting` teardown (clear `shadow_slot`, drop the shadow version's
  pages/heads/centroids), bounded across a subject sub-stage then a page sub-stage. `max_work` is
  clamped to `1..=MAX_REBUILD_STEP_WORK` for the same reason as the rebuild step.
- `admin_abort_vector_rebuild(index_id)` / `admin_vector_rebuild_status(index_id)` — abort returns
  straight to `Idle` from `Sampling`/`Training`/`Failed` (nothing persisted: `Training` keeps its
  centroids in the state record until the transition to `Building`) or enters `Aborting` from
  `Building`/`ReadyToPublish`; status is an O(1) scalar snapshot (never the candidate bytes), carrying
  the `Training` iteration count.

**Two-slot subject entry / atomic publish.** Publish stays metadata-only because the shadow live slot
lives in `SubjectMapEntry.shadow_slot` and search resolves the live slot via
`current_slot_for(def.active_index_version)`: the active `slot` while it matches, else the
`shadow_slot` once the atomic flip moves the active version onto the rebuilt one. Both read paths
(exact subject scan and partition-page scan) resolve through `current_slot_for`, so freshness is
never read off the wrong version — including the post-publish `Cleaning` window before a subject is
collapsed. `VECTOR_SUBJECT_TO_ID` therefore remains the single freshness source of truth; the
`VECTOR_ID_TO_SLOT` reverse locator tracks the active slot and is intentionally stale for
not-yet-collapsed subjects during `Cleaning` (search never relies on it), repointed at collapse.

**Dual-write semantics.** A mutation branches on the rebuild phase: active-only (no rebuild /
`Sampling` / `Training` / `Failed` / `Aborting`; a `Training`-era mutation is later shadowed when
`Building` walks every live subject), dual-write into active + shadow (`Building` / `ReadyToPublish`),
or active-only on the now-`target` version during `Cleaning`. In `Cleaning`, any **state-changing**
mutation collapses the touched subject (`slot = target`, `shadow_slot = None`, `VECTOR_ID_TO_SLOT`
repointed); a pure idempotent no-op (same version, identical bytes) changes nothing and is left for
`cleanup_step` to collapse. The active append uses centroid assignment whenever the active `nlist > 1`
(the published-index mutability path) and the shadow append always uses the target centroids.

### Slice 8 training quality + partition health (implemented)

A bounded, deterministic `Training` phase sits between `Sampling` and `Building`; the lifecycle is
now `Idle → Sampling → Training → Building → ReadyToPublish → Cleaning → Idle` (abort from
`Sampling`/`Training` is O(1) to `Idle`). No new stable region is added; the durable
`VectorRebuildStateRecord` gains a `Training` variant.

- **Bounded candidate pool (P2).** `Sampling` accumulates a *distinct* candidate pool capped by
  `candidate_pool_cap = min(byte_cap, op_cap)`, where `byte_cap = (MAX_REBUILD_STATE_BYTES -
  nlist * stride_bytes - MAX_REBUILD_STATE_OVERHEAD_BYTES) / stride_bytes` reserves room for the
  trained centroids and Candid encoding overhead inside the combined-state envelope, and `op_cap =
  MAX_REBUILD_TRAINING_DISTANCE_OPS / (nlist * dims)`. `MAX_REBUILD_STATE_BYTES` (8 MiB) is the
  **Candid-encoded** `to_bytes()` cap on the whole rebuild-state value, not a raw-vector-bytes cap.
- **Bounded per-message training work (P1).** `Training` performs exactly one k-means-lite iteration
  per `admin_vector_rebuild_step`: assign each candidate to its nearest current centroid (ties to the
  lowest id), recompute each centroid as the arithmetic mean of its members (an empty cluster keeps
  its previous centroid), `iteration += 1`. The pool cap guarantees
  `candidate_count * nlist * dims <= MAX_REBUILD_TRAINING_DISTANCE_OPS`; the per-iteration sums/counts
  are transient heap buffers (`O(nlist * dims)`), never persisted. After
  `MAX_REBUILD_TRAINING_ITERATIONS` it writes exactly `nlist` centroids to `IVF_CENTROIDS` and enters
  `Building`. `Training` writes no pages and no shadow slots.
- **Rebuild-state read cost (ADR 0033).** The candidate pool deliberately remains inside the
  `VECTOR_REBUILD_STATE` (MemoryId 12) record: measurement showed a contiguous-blob or dedicated-raw-region
  layout does not reduce the per-step cost, which is the repeated stable-memory read of the record. See
  [ADR 0033](../adr/0033-vector-rebuild-state-read-memoization.md) for the rejected layout changes and the
  proposed transient heap memoization of `rebuild_state_of`.
- **Fail-closed encoded-size guard (single encode).** Before persisting any `Training` value (the
  `Sampling → Training` transition and each post-iteration `Training → Training` re-persist) the
  canister Candid-encodes the value **once**, checks that length against `MAX_REBUILD_STATE_BYTES`,
  and — if within budget — stores those same bytes verbatim via `RawRebuildState` rather than
  re-encoding on insert. An oversized value returns `InvalidRebuildParams` rather than
  `assert!`/trapping the message and leaves the prior recoverable state intact. The conservative pool
  cap normally keeps this from firing; the guard absorbs Candid-overhead drift. The `RawRebuildState`
  wrapper stores the exact `VectorRebuildStateRecord` Candid bytes (on-disk format unchanged), so the
  guard and the persist share one encode and `rebuild_state_of` decodes once per step.
- **Partition health.** `admin_vector_partition_health(index_id)` (Router-guarded `#[query]`) returns
  a head-only, integer-only `VectorPartitionHealthSummary { nlist, partitions_examined, live_rows,
  page_count, max_partition_live_rows }`. It reads the active version's `0..nlist` `PartitionHead`
  rows (O(`nlist`), bounded by `MAX_NLIST`, no page scan); the caller derives
  `avg = live_rows / nlist` and the skew ratio `max_partition_live_rows / avg`. Tombstone accounting
  is deferred to Slice 9+ (it would need a page scan or new persisted counters). Search is unchanged
  (no `nprobe` on the wire, no mid-scan truncation).

### Slice 9 maintenance visibility + centroid cache (implemented)

Maintenance/cache surface only — no change to canonical ownership, search semantics, or stable layout
(heap-only cache; reuses `VECTOR_PAGE_META`, `VECTOR_PARTITION_HEADS`, `IVF_CENTROIDS`). All new admin
endpoints stay on the vector canister behind `guard_router_canister` (driven by the router principal);
Router forwarding landed in Slice 10 (below).

- **Bounded page-meta tombstone health.** `admin_vector_partition_health_step(index_id, cursor,
  max_pages)` (Router-guarded `#[query]`) scans at most `max_pages` `VECTOR_PAGE_META` entries of the
  active `(index_id, active_index_version)` — page meta only, never row bytes or `VECTOR_SUBJECT_TO_ID`
  — returning an additive `VectorPartitionHealthStep { partial: VectorPartitionPageHealth { index_id,
  index_version, page_count, total_rows, physical_live_rows, tombstoned_rows }, cursor, exhausted }`.
  Callers repeat until `exhausted` and sum the partials (the `VectorSlabStatsStep` merge /
  no-snapshot-isolation contract); `max_pages` is clamped server-side and a malformed or wrong-scope
  cursor returns `InvalidStatsCursor` rather than trapping. This complements the head-only Slice 8 skew
  summary with the tombstone signal the head cannot see.
- **Recommendation + trigger (no autonomous timer).** `recommend_partition_maintenance(summary,
  page_health, policy)` is a pure function returning `VectorMaintenanceRecommendation { Healthy,
  RebuildRecommended, RebuildRequired }` as the max severity across two independently min-row-gated
  signals — tombstone ratio (`tombstoned_rows / total_rows`) and partition skew
  (`max_partition_live_rows * nlist / live_rows`) — compared against a `VectorMaintenancePolicy`'s split
  `recommended_*_bps`/`required_*_bps` thresholds with `u128` cross-multiplication (no floats, no
  overflow; an inverted policy returns `InvalidMaintenancePolicy`).
  `admin_start_vector_rebuild_if_recommended(index_id, attested_page_health, policy, target_nlist,
  sample_limit)` (Router-guarded `#[update]`) recomputes the head-only skew `summary` server-side from
  the authoritative partition heads (O(`nlist`)), re-derives the recommendation, and, when not
  `Healthy`, begins an existing rebuild, returning the recommendation. The skew summary thus has no
  caller-trust surface; only the page-meta tombstone health is *trusted admin input*. A generation
  guard rejects page health attested against a different generation (`attested_page_health.index_id`/
  `index_version` must equal the active version, else `StaleMaintenanceHealth`). `target_nlist = None`
  defaults to `def.nlist` only when `>= 2` (degenerate `nlist = 1` requires an explicit target).
- **Heap centroid cache.** A transient `thread_local` cache keyed by `(index_id ->
  {version, nlist, dims})` memoizes the decoded centroid set so the partition-page `#[query]` search
  skips the `IVF_CENTROIDS` stable read + `f32` decode. IC `#[query]` execution is non-committing, so
  the query path is **read-only**: a warmed entry is used; a miss reads stable for that call only and
  does **not** populate the cache. Population/eviction are `#[update]`-only —
  `admin_vector_centroid_cache_warmup(index_id)` (caches a ready `nlist > 1` set; drops stale entries
  for degenerate/untrained indexes), `admin_vector_centroid_cache_clear()`, and a publish-time
  invalidation when the active generation flips; the cache is byte-bounded and dropped on init/upgrade.
  `admin_vector_centroid_cache_status()` reports `VectorCentroidCacheStatus { entries, bytes,
  max_bytes }` — per-query hit/miss is intentionally not tracked (a query cannot commit counters on IC).
  Canbench shows the warm partition-page search is measurably cheaper than cold (e.g. d768/nlist64:
  ~98.1M cold vs ~92.0M instructions warm).

### Slice 10 maintenance orchestration (implemented)

Slice 10 makes the Slice 7-9 maintenance surface operable from the Router without moving execution
state out of the vector canister. The boundary is: **Router owns policy + authority; the vector
canister owns derived maintenance execution.** It adds one stable region per canister and changes no
search/dual-write/publish semantics.

- **Vector-owned execution state (`VECTOR_MAINTENANCE_STATE`, MemoryId 14).** A per-index
  `VectorMaintenanceState` — `Idle`; `Scanning { cursor: Option<Vec<u8>>, exhausted: bool, merged:
  VectorPartitionPageHealth }` (`exhausted` is an explicit phase flag, never encoded via
  `cursor == None`); `Failed(VectorMaintenanceFailure { code, message })` (message truncated to a
  bounded length so persisted size is error-string-independent). Unlike the heap-only centroid cache,
  this is durable execution state: it **persists across upgrade** and is cleared only on canister
  init/reset.
- **Bounded vector step.** `admin_vector_maintenance_step(index_id, VectorMaintenanceStepRequest {
  policy, target_nlist, sample_limit, scan_max_pages, rebuild_max_subjects, cleanup_max_work })`
  (Router-guarded `#[update]`) advances exactly one bounded unit: it drives an in-flight
  rebuild/cleanup first; otherwise from `Idle` it starts a scan and does one
  `partition_page_health_step`; when the scan exhausts it validates the merged generation against the
  active version, recomputes the head summary, and runs `recommend_partition_maintenance`, starting a
  rebuild on `Recommended`/`Required`. Two generation guards keep it correct across an active-version
  flip: a stale cursor mid-scan (`InvalidStatsCursor`) restarts from the lower bound, and the
  exhausted→recommend boundary re-checks `merged.index_version == active_index_version`. It **stops at
  `ReadyToPublish`** (returns `AwaitingPublish`) — publish stays explicit.
  `admin_vector_maintenance_status` (query) and `admin_vector_maintenance_reset(index_id)` (update;
  `Idle` from any state including `Failed`, without touching the rebuild state) round out the surface.
- **Router policy SSOT (`ROUTER_VECTOR_MAINTENANCE_POLICIES`, MemoryId 44).** A per-`(graph_id,
  index_id)` `VectorMaintenancePolicyRecord { enabled, policy, target_nlist, sample_limit,
  scan_max_pages, rebuild_max_subjects, cleanup_max_work }`, **absent/disabled by default**. Authorship
  (`admin_set_/disable_/delete_vector_maintenance_policy`, validated for `recommended_*_bps <=
  required_*_bps`, nonzero budgets, and an existing definition) is `authorize_index_ddl`; stepping,
  reads, and reset use a new Admin-only `authorize_vector_maintenance`.
- **Router forwarding.** The Router exposes the whole Slice 7-9 maintenance surface as forwards to the
  resolved single target (reads as composite queries, mutators/drivers as updates), each gated on
  resolve + non-anonymous target + dispatch readiness. The push step `admin_vector_maintenance_step(
  graph, index_id)` returns `Disabled` when no enabled policy exists, otherwise snapshots the policy
  and forwards one bounded unit; `vector_maintenance_status(graph, index_id)` reports Router
  policy/readiness plus the forwarded execution + rebuild state (cursors present/absent, not decoded).
  Future automatic mode (vector-index-pull from Router policy) is documented but not implemented.

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


## Filtered exact ranking (ADR 0034 Slices 6, 7, 8, 9, 10, 11, 12, 13 and 14)

A bounded candidate allowlist can restrict the search to an exact top-k over current live vector
slots. The allowlist is produced by the Router from the Property Index for both leading and
non-leading `SEARCH ... WHERE` predicates (one equality, one to eight `AND`-connected same-binding
equalities on distinct properties, one same-binding numeric range predicate, exactly two
same-binding numeric range predicates on the same property (one lower `>`/`>=` and one upper
`<`/`<=`) forming a two-sided range, or one to eight equality predicates on distinct properties
plus one one- or two-sided numeric range predicate on a distinct property) and arrives in `VectorSearchRequest.candidate_subjects`.
Router intersects the two range arms into one encoded interval before issuing the allowlist; Vector Index behavior is unchanged:

- `None` keeps the existing unrestricted search path (exact subject scan or partition-page scan).
- `Some([])` returns an empty result without reading vector rows.
- A non-empty allowlist is validated at the receiving boundary: the count must not exceed
  `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` (4096), every subject must be a vertex, and duplicates are
  rejected with `InvalidSearchCandidates`.
- For each allowed subject, the canister resolves the current slot via `VECTOR_SUBJECT_TO_ID`,
  re-reads the row through the slab page store, re-validates `vector_id` / `generation` / `slot`
  consistency, and scores the row with the existing metric exact path. Deleted, stale, superseded, or
  otherwise inconsistent subjects are skipped.
- Qualifying rows are pushed through the same bounded top-k heap and deterministic tie-breaking used
  by unrestricted search, so the result is the exact top-k over the allowlist.
- The allowlist is transient: no new stable-memory region is introduced, and property values are not
  copied into the vector canister.

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

Each page is both a storage unit and a scoring unit. Per [ADR 0032](../adr/0032-vector-index-slab-page-store.md)
a page is a `VECTOR_PAGE_META` directory entry over a fixed-stride span of the raw `VECTOR_ROW_SLAB`,
laid out structure-of-arrays so the vector bytes form one contiguous scan unit, separated from the
per-row metadata tables:

```text
VECTOR_PAGE_META[(index_id, version, partition_id, page_id)] ->
  { slab_offset, capacity, row_count, live_count, row_stride, tombstone_count }

VECTOR_ROW_SLAB @ slab_offset ->
  page header { page_magic, capacity, row_stride }
  vector_id       [u64; capacity]
  generation      [u64; capacity]
  subject_locator [(shard_id, vertex_id); capacity]
  tombstone_bits  [ceil(capacity / 8)]
  vector_bytes    [capacity * row_stride]
```

The first derived store supports `F32` only. The structure is still encoding-aware: different
dimensions or encodings use different indexes or physical page families, mirroring the LabeledLARA
pattern where owner metadata fixes the byte width before reading. Vector ids are not reused in the
first implementation; deletes set tombstone bits so stale slot references remain safe until cleanup.

Updates append a new slot and tombstone the old slot. Search reads a selected page's slab span into
a reused heap scratch buffer and performs SIMD exact scoring over the contiguous `vector_bytes`
table.

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
VECTOR_PAGE_META[(index_id, version, partition_id, page_id)] ->
  { slab_offset, capacity, row_count, live_count, row_stride, tombstone_count }
VECTOR_ROW_SLAB -> raw structure-of-arrays row bytes addressed by VECTOR_PAGE_META.slab_offset
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
| 1 | IVF_FLAT (`ivf_flat`) | exact scan implemented (Slice 5); centroid routing + `nprobe` partition-page scan implemented (Slice 6); production bounded shadow-version rebuild + dual-write + atomic publish implemented (Slice 7); bounded k-means-lite training + head-only partition health implemented (Slice 8); bounded page-meta tombstone health + rebuild recommendation/trigger + heap centroid cache implemented (Slice 9); Router-forwarded maintenance + Router policy catalog + vector-owned bounded maintenance step implemented (Slice 10); full/balanced k-means + autonomous tombstone cleanup planned (Slice 11+) | standard vector index: centroid routing, partition pages, query-aware pruning, exact rerank |
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
- [done, slice 9] Define centroid cache miss behavior: the heap cache is read-only on the `#[query]`
  search path (a miss reads stable for that call only, no population); warmup/clear/invalidation are
  `#[update]`-only.
- Define shadow-version rebuild, balanced assignment, publish, and cleanup.
- Define partition tombstone cleanup thresholds. [partial, slice 9/10] Caller-supplied
  `VectorMaintenancePolicy` thresholds (tombstone ratio + skew, split recommended/required bps) drive
  the `recommend_partition_maintenance` / `admin_start_vector_rebuild_if_recommended` trigger (Slice 9),
  and Slice 10 makes them a **durable Router-owned policy catalog** (disabled by default) forwarded one
  bounded step at a time; autonomous (timer-driven) cleanup scheduling remains Slice 11+.
- [done, slice 5] Add canbench targets for exact `ivf_flat` search (`crates/graph-vector-index`,
  dims 128/384/768 × top_k 10/100).
- [done, slice 6] Add canbench targets for the partition-page scan over clustered seeded datasets
  (dims 128/384/768 × `nlist` 16/64 × `nprobe` 1/4/8/16): `nprobe = nlist` is the exact-parity upper
  bound (matching result set, higher instruction cost than exact due to centroid + reverse-map
  lookups), and lower `nprobe` measurably reduces cost.
- [done, slice 7] Add canbench targets for the production rebuild: full rebuild (start → bounded steps
  → publish) at dims 128/384/768, an isolated `Building` shadow-append step, and normal vs dual-write
  upsert cost (dual-write is ~2× a normal upsert since it writes both versions).
- [done, slice 8] Add canbench targets for an isolated k-means-lite `Training` iteration over the full
  candidate pool (dims 128/384/768 × `nlist` 16/64); the full-rebuild targets now also cover the
  training cost end to end.
- [done, slice 9] Add canbench targets for the bounded page-meta health scan (clean and
  tombstone-heavy), cold-vs-warm partition-page search with the heap centroid cache (dims 128/768 ×
  `nlist` 64 × `nprobe` 8), and the centroid-cache warmup cost.

## Related documents

- [ADR 0031](../adr/0031-vertex-embedding-store-and-derived-vector-index.md)
- [property-index.md](property-index.md)
- [derived-state-query-semantics.md](derived-state-query-semantics.md)
- [capacity-planning.md](capacity-planning.md)
- [../architecture/overview.md](../architecture/overview.md)
- [../storage/labeled-edge-payloads.md](../storage/labeled-edge-payloads.md)
- [../storage/payload-first-traversal.md](../storage/payload-first-traversal.md)
