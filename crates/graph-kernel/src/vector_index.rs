//! Shared vector-index types.
//!
//! Per [ADR 0031](design/adr/0031-vertex-embedding-store-and-derived-vector-index.md), this module
//! is the home for vector-index wire types. Slice 1 carried only the canonical embedding encoding.
//! Slice 2 adds the derived sync/mutation wire surface (`VectorIndexKind`, `VectorMetric`,
//! `VectorSubject`, `VectorEmbeddingSyncOp`, `IndexedEmbeddingCatalog`, `VectorIndexError`).
//! Search/cursor types are deliberately deferred to Slice 5+ (Router catalog + target resolution
//! is Slice 3; incarnation-fenced production activation is Slice 4; search/centroids are Slice 5+).
//!
//! # Version naming glossary
//!
//! Four distinct concepts that are never conflated in code or wire:
//!
//! - `embedding_incarnation` (graph canonical store, ADR 0031 Slice 4): delete-spanning ordering
//!   fence per `(VertexId, EmbeddingNameId)` identity. Strictly increases across each delete/reinsert
//!   and is never reset. The vector canister orders sync ops by `(embedding_incarnation,
//!   embedding_version)`, so a stale remove cannot tombstone a newer live vector.
//! - `embedding_version` (graph canonical store): `StoredEmbedding.version`; the per-incarnation
//!   update counter (resets to `1` on each fresh incarnation), carried on sync ops and the repair
//!   journal and consulted only within an incarnation for sync/repair idempotence.
//! - `index_version` (vector-index canister): physical index generation; page/partition head keys.
//! - `generation` (vector-index canister): slot/entity handle incarnation for append-and-tombstone.

use crate::federation::ShardId;
use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Encoding of a stored vertex embedding.
///
/// Only fixed-dimension `F32` is supported in the first slice. New variants (`F16`, `I8`) must
/// update every exhaustive `match` on this enum, which is the intended compile-time gate before
/// an `UnsupportedEncoding`-style runtime branch is introduced.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub enum VectorEncoding {
    /// IEEE-754 little-endian `f32` components; byte width is `dims * 4`.
    F32,
}

impl VectorEncoding {
    /// Byte width of one component for this encoding.
    pub const fn component_bytes(self) -> u32 {
        match self {
            Self::F32 => 4,
        }
    }

    /// Byte width (`stride`) of a full vector with `dims` components.
    pub const fn stride_bytes(self, dims: u16) -> u32 {
        self.component_bytes() * dims as u32
    }
}

/// Physical index structure for a derived vector index.
///
/// Slice 2 standardizes on `IvfFlat` operated in its degenerate form (`nlist = 1`,
/// `partition_id = 0`, no centroids). There is intentionally no separate `Flat` kind: the
/// baseline exact scan landed in Slice 4+ is `IvfFlat` with one partition.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub enum VectorIndexKind {
    /// Inverted-file flat: centroid-pruned exact rerank. Degenerate `nlist = 1` in Slice 2.
    IvfFlat,
}

/// Distance/similarity metric for vector scoring.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub enum VectorMetric {
    /// Squared Euclidean distance (no square root); smaller is nearer.
    L2Squared,
    /// Cosine similarity expressed internally as `1 - similarity`; smaller is nearer.
    /// User-facing score is `1 - raw`; this metric has no natural distance.
    Cosine,
}

/// Whether a metric exposes a distance or a score to the user-facing GQL surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VectorOutputShape {
    /// Smaller values are nearer; e.g. `L2Squared`.
    Distance,
    /// Larger values are better; e.g. `Cosine`.
    Score,
}

impl VectorMetric {
    /// Classification of this metric on the GQL `SEARCH` output surface.
    pub const fn output_shape(self) -> VectorOutputShape {
        match self {
            Self::L2Squared => VectorOutputShape::Distance,
            Self::Cosine => VectorOutputShape::Score,
        }
    }

    /// Convert an internal raw value (smaller is nearer) to a user-facing distance, if this metric
    /// has one. Returns `None` for non-finite `raw` or for metrics that do not expose distance.
    pub fn to_user_distance(self, raw: f32) -> Option<f32> {
        if !raw.is_finite() {
            return None;
        }
        match self {
            Self::L2Squared => Some(raw),
            Self::Cosine => None,
        }
    }

    /// Convert an internal raw value (smaller is nearer) to a user-facing score, if this metric
    /// has one. Returns `None` for non-finite `raw` or for metrics that do not expose score.
    pub fn to_user_score(self, raw: f32) -> Option<f32> {
        if !raw.is_finite() {
            return None;
        }
        match self {
            Self::L2Squared => None,
            // internal raw = 1 - similarity; user score = similarity.
            Self::Cosine => Some(1.0 - raw),
        }
    }
}

/// What a stored vector refers to.
///
/// Slice 2 supports only graph vertices. `shard_id` is carried inside the subject so the
/// subject-map key is `(index_id, subject)` with no separate `shard_id` field; the canister
/// validates `shard_id` against the caller's attached shard. `VectorSubject::Edge` is deferred.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub enum VectorSubject {
    /// A graph vertex identified by its owning shard and shard-local id.
    Vertex { shard_id: ShardId, vertex_id: u32 },
}

impl VectorSubject {
    /// The owning shard of this subject.
    pub const fn shard_id(self) -> ShardId {
        match self {
            Self::Vertex { shard_id, .. } => shard_id,
        }
    }
}

/// Graph shard → vector-index canister: one derived embedding mutation.
///
/// `bytes` is REQUIRED for an upsert (`remove = false`) and EMPTY for a remove (`remove = true`);
/// idempotence is decided by the ordered pair `(embedding_incarnation, embedding_version)` against
/// the retained subject clock and never reads `bytes`. `encoding`/`dims` on a remove op are ignored
/// by the canister.
///
/// Contract (ADR 0031 Slice 4): `embedding_incarnation > 0`; an upsert carries `embedding_version >
/// 0`; a remove carries the deleted record's incarnation and an empty `bytes`. No in-flight ops
/// predate this field in production (dispatch was inert before activation), so it is a required
/// field rather than an `Option`.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct VectorEmbeddingSyncOp {
    pub index_id: u32,
    /// Routing filter; resolved against the Router catalog at activation (Slice 3+).
    pub embedding_name_id: u16,
    pub subject: VectorSubject,
    /// Graph-owned delete-spanning ordering fence (ADR 0031 Slice 4). Strictly increases across each
    /// delete/reinsert of the identity; the canister orders by `(embedding_incarnation,
    /// embedding_version)`.
    pub embedding_incarnation: u64,
    /// Canonical `StoredEmbedding.version` from the graph `VertexEmbeddingStore`; the per-incarnation
    /// update counter.
    pub embedding_version: u64,
    pub encoding: VectorEncoding,
    pub dims: u16,
    /// Metric of the target index definition.
    pub metric: VectorMetric,
    /// REQUIRED for upsert; EMPTY for remove — never read for idempotence.
    pub bytes: Vec<u8>,
    pub remove: bool,
}

/// One indexed embedding definition supplied ephemerally by the Router (Slice 3).
///
/// Slice 2 defines the type only; the graph never persists an indexed-embedding registry. A
/// dispatch with no installed catalog skips vector sync entirely (production), while tests inject
/// a catalog via the embedding catalog context.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub struct IndexedEmbeddingSpec {
    pub embedding_name_id: u16,
    pub index_id: u32,
    pub kind: VectorIndexKind,
    pub metric: VectorMetric,
    pub encoding: VectorEncoding,
    pub dims: u16,
}

/// Router-sourced snapshot of which embedding names are indexed (mirrors `IndexedPropertyCatalog`).
#[derive(Clone, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct IndexedEmbeddingCatalog {
    pub embeddings: Vec<IndexedEmbeddingSpec>,
}

impl IndexedEmbeddingCatalog {
    pub fn is_empty(&self) -> bool {
        self.embeddings.is_empty()
    }

    /// The indexed spec for an embedding name, if registered.
    pub fn spec_for(&self, embedding_name_id: u16) -> Option<IndexedEmbeddingSpec> {
        self.embeddings
            .iter()
            .copied()
            .find(|spec| spec.embedding_name_id == embedding_name_id)
    }
}

/// Upper bound on `top_k` accepted by a single vector search (ADR 0031 Slice 5). Bounds the
/// in-heap candidate set and result size so one query stays within the canister instruction budget.
pub const MAX_VECTOR_SEARCH_TOP_K: u32 = 1024;

/// Upper bound on the number of distinct candidate subjects supplied to a filtered vector search
/// (ADR 0034 Slice 6). The Router collects at most this many subjects from Property Index before
/// failing closed; the Vector Index receiving boundary revalidates the count.
pub const MAX_VECTOR_SEARCH_FILTER_CANDIDATES: usize = 4096;

/// Read-only exact top-k vector search over a derived `ivf_flat` index (ADR 0031 Slice 5).
///
/// `query` carries `dims` components encoded as `encoding` (`encoding.stride_bytes(dims)` bytes).
/// Slice 5 is the degenerate exact baseline: `encoding == F32`, `metric == L2Squared`, single
/// partition, exact scoring; centroid pruning / `nprobe` arrive in Slice 6+.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct VectorSearchRequest {
    pub index_id: u32,
    /// `dims * encoding.component_bytes()` bytes of the query vector.
    pub query: Vec<u8>,
    pub encoding: VectorEncoding,
    pub dims: u16,
    pub metric: VectorMetric,
    /// Number of nearest neighbors to return; `0 < top_k <= MAX_VECTOR_SEARCH_TOP_K`.
    pub top_k: u32,
    /// Optional bounded candidate allowlist (ADR 0034 Slice 6). When `Some`, the result is the
    /// exact top-k over only these subjects resolved against current live vector slots. `None`
    /// keeps the existing unrestricted search semantics. `Some([])` returns no hits.
    pub candidate_subjects: Option<Vec<VectorSubject>>,
}

/// One scored search result. `distance` is the metric-specific internal raw value (smaller is
/// nearer), not necessarily a public distance. For `L2Squared` it is the squared Euclidean distance;
/// for `Cosine` it is `1 - cosine_similarity`. The Router converts this raw value to the
/// user-facing scalar requested by `SCORE AS` or `DISTANCE AS`. `embedding_incarnation` /
/// `embedding_version` are the live subject clock so a caller can reason about freshness
/// (ADR 0031 Slice 4).
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct VectorSearchHit {
    pub subject: VectorSubject,
    pub distance: f32,
    pub embedding_incarnation: u64,
    pub embedding_version: u64,
}

/// Top-k search result, ordered by `(distance ascending, subject ascending)` as a deterministic
/// tie-breaker.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct VectorSearchResult {
    pub hits: Vec<VectorSearchHit>,
}

/// Phase tag of a per-index rebuild lifecycle (ADR 0031 Slice 7). Mirrors the durable
/// `VectorRebuildStateRecord` but carries no cursors or per-subject collections — only a bounded
/// scalar snapshot for the admin status query.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub enum VectorRebuildPhase {
    Idle,
    Sampling,
    /// Deterministic k-means-lite centroid refinement over the bounded candidate pool (ADR 0031
    /// Slice 8), between `Sampling` and `Building`.
    Training,
    Building,
    ReadyToPublish,
    Cleaning,
    Aborting,
    Failed,
}

/// Bounded scalar snapshot of a rebuild's progress (ADR 0031 Slice 7).
///
/// The response is O(1): it never carries candidate centroid bytes or any per-subject collection,
/// so a status query stays within a fixed reply budget regardless of index size.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct VectorRebuildStatus {
    pub phase: VectorRebuildPhase,
    /// The shadow (target) index version being built, or `0` when `Idle`.
    pub target_index_version: u64,
    /// The target `nlist` of the rebuild (`Sampling`/`Building`/`ReadyToPublish`/`Aborting`), the
    /// old `nlist` during `Cleaning`, or `0` when `Idle`/`Failed`.
    pub nlist: u32,
    /// Subjects shadowed so far during `Building` (`0` in other phases).
    pub subjects_processed: u64,
    /// Distinct centroid candidates collected so far during `Sampling`/`Training` (`0` in other
    /// phases).
    pub candidates_collected: u32,
    /// Completed k-means-lite iterations during `Training` (`0` in other phases, ADR 0031 Slice 8).
    pub training_iteration: u32,
}

/// Bounded, head-only partition-health summary for an `ivf_flat` index (ADR 0031 Slice 8).
///
/// O(`nlist`) over the active version's `PartitionHead` rows (no page scan, bounded by `MAX_NLIST`).
/// Reports integer-only raw counts; callers derive `avg_live_rows = live_rows / nlist` and the skew
/// ratio `max_partition_live_rows / avg_live_rows` themselves. This summary stays intentionally
/// head-only: tombstone accounting (`tombstoned_rows`/`total_rows`/tombstone ratio) requires a page
/// scan and is provided separately by the Slice 9 page-meta health type
/// ([`VectorPartitionPageHealth`], accumulated via [`VectorPartitionHealthStep`]).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub struct VectorPartitionHealthSummary {
    /// Configured partition count of the active index version.
    pub nlist: u32,
    /// Partitions with a materialized `PartitionHead` (an empty partition materializes no head).
    pub partitions_examined: u32,
    /// Sum of `live_len` across examined partitions.
    pub live_rows: u64,
    /// Sum of `page_count` across examined partitions.
    pub page_count: u64,
    /// Largest single-partition `live_len` (skew numerator).
    pub max_partition_live_rows: u64,
}

/// Bounded page-meta tombstone accounting for one `(index_id, active index version)` (ADR 0031
/// Slice 9).
///
/// Derived from the `VECTOR_PAGE_META` directory only (no row-byte read, no `VECTOR_SUBJECT_TO_ID`
/// read), it complements the head-only [`VectorPartitionHealthSummary`] (which owns the skew
/// signal) with the tombstone signal the head cannot see. Integer-only; callers derive the tombstone
/// ratio as `tombstoned_rows / total_rows`.
///
/// `physical_live_rows` is `VectorPageMeta.live_count` (physical non-tombstone rows); it is **not**
/// subject-freshness and can exceed the searchable count because the search freshness check skips
/// stale/meta-drift rows (mirrors [`VectorSlabScopeStats::physical_live_row_count`]).
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    CandidType,
    Serialize,
    Deserialize,
)]
pub struct VectorPartitionPageHealth {
    /// Owning index.
    pub index_id: u32,
    /// The active index version the scan was scoped to.
    pub index_version: u64,
    /// Page-meta entries observed for `(index_id, index_version)`.
    pub page_count: u64,
    /// Physical rows (live + tombstoned) summed over those pages.
    pub total_rows: u64,
    /// Physical non-tombstone rows (`VectorPageMeta.live_count`); not subject-freshness.
    pub physical_live_rows: u64,
    /// Tombstoned rows summed over those pages.
    pub tombstoned_rows: u64,
}

/// One bounded page-meta health scan step for `admin_vector_partition_health_step` (ADR 0031
/// Slice 9). IC-safe incremental scan scoped to one `(index_id, active index version)`, mirroring
/// [`VectorSlabStatsStep`].
///
/// **Merge contract.** `partial`'s counters (`page_count`/`total_rows`/`physical_live_rows`/
/// `tombstoned_rows`) are *additive* across steps; `index_id`/`index_version` are repeated (take any
/// step's value). Callers repeat until `exhausted` and sum the counters.
///
/// `cursor` is opaque `PageKey` bytes scoped to this `(index_id, index_version)`; pass it back
/// verbatim. It is `None` exactly when `exhausted`.
///
/// **No snapshot isolation.** This is a bounded best-effort scan, not a point-in-time snapshot:
/// concurrent `VECTOR_PAGE_META` writes between steps are not isolated (a page inserted before the
/// cursor is missed, a counted-then-deleted page lingers in the merge). Run during a quiescent window
/// for an exact figure; the diagnostic counters tolerate small cross-call drift otherwise.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct VectorPartitionHealthStep {
    /// This step's additive contribution (see the merge contract above).
    pub partial: VectorPartitionPageHealth,
    /// Opaque resume cursor (`PageKey` bytes); `None` exactly when `exhausted`.
    pub cursor: Option<Vec<u8>>,
    /// `true` once every page of the scoped `(index_id, index_version)` has been scanned.
    pub exhausted: bool,
}

/// Caller-supplied thresholds for [`recommend_partition_maintenance`]-style maintenance decisions
/// (ADR 0031 Slice 9). Not persisted in Slice 9.
///
/// Two thresholds per signal so the three-state [`VectorMaintenanceRecommendation`] is well defined:
/// crossing `required_*` is `RebuildRequired`, crossing only `recommended_*` is `RebuildRecommended`.
/// Both ratios are basis points (1 bp = 1/10000). The recommendation function rejects a policy where
/// `recommended_*_bps > required_*_bps`.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    CandidType,
    Serialize,
    Deserialize,
)]
pub struct VectorMaintenancePolicy {
    /// Tombstone ratio (`tombstoned_rows / total_rows`, bps) at/above which a rebuild is recommended.
    pub recommended_tombstone_ratio_bps: u32,
    /// Tombstone ratio (bps) at/above which a rebuild is required.
    pub required_tombstone_ratio_bps: u32,
    /// Skew ratio (`max_partition_live_rows / avg_live_rows`, bps) at/above which a rebuild is
    /// recommended.
    pub recommended_skew_ratio_bps: u32,
    /// Skew ratio (bps) at/above which a rebuild is required.
    pub required_skew_ratio_bps: u32,
    /// Minimum `total_rows` before either signal is judged (too small to judge below this).
    pub min_total_rows: u64,
    /// Minimum `tombstoned_rows` before the tombstone signal is judged (skew is not gated by this).
    pub min_tombstoned_rows: u64,
}

/// Deterministic maintenance recommendation from merged partition health (ADR 0031 Slice 9).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub enum VectorMaintenanceRecommendation {
    /// No maintenance needed (or too little data to judge).
    Healthy,
    /// At least one signal crossed its `recommended` threshold (but none its `required`).
    RebuildRecommended,
    /// At least one signal crossed its `required` threshold.
    RebuildRequired,
}

/// Bounded failure detail persisted in [`VectorMaintenanceState::Failed`] (ADR 0031 Slice 10).
///
/// `message` is truncated by the canister so the persisted maintenance-state size never depends on a
/// downstream error string (mirrors the Slice 7/8 rebuild-state byte-cap discipline).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct VectorMaintenanceFailure {
    /// The canister error that aborted the maintenance step.
    pub code: VectorIndexError,
    /// Human-readable detail, truncated to a bounded length.
    pub message: String,
}

/// Vector-canister-owned maintenance execution state for one index (ADR 0031 Slice 10).
///
/// Only the **scan** phase is tracked here; once a rebuild starts, `VECTOR_REBUILD_STATE` is the
/// source of truth for the rebuild/cleanup phases and this returns to `Idle`. Persisted in
/// `VECTOR_MAINTENANCE_STATE` and **survives upgrade** (it holds mid-orchestration scan progress); it
/// is cleared only on canister init/reset, never on upgrade (unlike the heap centroid cache).
///
/// `exhausted` is an explicit phase flag, **not** encoded via `cursor == None`: `cursor = None,
/// exhausted = false` means "(re)start the scan from the lower bound", while `exhausted = true` means
/// "scan complete; recommend only after generation validation". `merged` carries the scoped
/// `index_id`/`index_version` the scan accumulated against, so an active-version flip after exhaustion
/// is detectable before recommending.
#[derive(Clone, Debug, PartialEq, Eq, Default, CandidType, Serialize, Deserialize)]
pub enum VectorMaintenanceState {
    /// No maintenance in progress.
    #[default]
    Idle,
    /// A bounded page-health scan is accumulating tombstone counters for the active version.
    Scanning {
        /// Opaque resume cursor (`PageKey` bytes); `None` restarts the scan from the lower bound.
        cursor: Option<Vec<u8>>,
        /// `true` once the scan has covered every page of the scoped version.
        exhausted: bool,
        /// Additive page-health accumulated so far, scoped by its `index_id`/`index_version`.
        merged: VectorPartitionPageHealth,
    },
    /// A prior step failed; the step is a no-op until an explicit `admin_vector_maintenance_reset`.
    Failed(VectorMaintenanceFailure),
}

/// Router-snapshotted policy + per-step budgets forwarded to `admin_vector_maintenance_step` (ADR
/// 0031 Slice 10).
///
/// The vector canister treats this as **step input**, never a durable copied SSOT — the Router owns
/// the maintenance policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct VectorMaintenanceStepRequest {
    /// Thresholds evaluated when a scan exhausts (see [`VectorMaintenancePolicy`]).
    pub policy: VectorMaintenancePolicy,
    /// Rebuild `nlist`; `None` defaults to the current `def.nlist` only when it is `>= 2`.
    pub target_nlist: Option<u32>,
    /// Rebuild sampling limit forwarded to the rebuild start.
    pub sample_limit: u32,
    /// Max page-meta entries scanned in one bounded scan step.
    pub scan_max_pages: u32,
    /// Max subjects processed in one bounded rebuild step.
    pub rebuild_max_subjects: u32,
    /// Max work units processed in one bounded cleanup/abort step.
    pub cleanup_max_work: u32,
}

/// Outcome of one bounded `admin_vector_maintenance_step` unit (ADR 0031 Slice 10).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum VectorMaintenanceStepResult {
    /// Advanced (or restarted) the page-health scan; `exhausted` once the scan is complete.
    Scanning {
        /// Whether the scan has now covered every page of the scoped version.
        exhausted: bool,
    },
    /// The exhausted scan judged the index healthy; maintenance state reset to `Idle`.
    Healthy,
    /// A rebuild was started from the crossed recommendation.
    RebuildStarted(VectorMaintenanceRecommendation),
    /// Drove one bounded rebuild step (`Sampling`/`Training`/`Building`, or a `Failed` rebuild phase).
    RebuildAdvanced(VectorRebuildStatus),
    /// The rebuild reached `ReadyToPublish`; publish is an explicit, separately-forwarded operation.
    AwaitingPublish(VectorRebuildStatus),
    /// Drove one bounded post-publish `Cleaning` (or `Aborting`) teardown step.
    CleanupAdvanced(VectorRebuildStatus),
    /// A step failed; recover with `admin_vector_maintenance_reset` (and abort the rebuild if needed).
    Failed(VectorMaintenanceFailure),
}

/// Bounded status of the heap centroid cache (ADR 0031 Slice 9).
///
/// Per-query hit/miss counts are intentionally **not** reported: `vector_search` is a `#[query]` and
/// IC query execution is non-committing, so a query cannot truthfully maintain cache counters. Only
/// the durable heap facts maintained by the update warmup/clear paths are reported.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    CandidType,
    Serialize,
    Deserialize,
)]
pub struct VectorCentroidCacheStatus {
    /// Number of cached centroid sets (one per warmed `(index_id, version, nlist, epoch)`).
    pub entries: u64,
    /// Total heap bytes the cached centroid sets occupy.
    pub bytes: u64,
    /// Configured byte cap of the cache.
    pub max_bytes: u64,
}

/// Derived, admin-only slab-space observability for the ADR 0032 vector slab page store.
///
/// **Maintenance observation, not search truth.** Computed purely from `VECTOR_PAGE_META` plus the
/// slab header; it never reads row bytes or `VECTOR_SUBJECT_TO_ID`, and never feeds search,
/// mutation, rebuild, or freshness decisions. Dead-space figures are approximate and intentionally
/// conservative.
///
/// `slab` holds whole-slab physical facts that are always global (the `VECTOR_ROW_SLAB` region is a
/// single allocation domain shared by every index/version), even when a query scopes the logical
/// counters to one `index_id`. `scope` and `versions` carry the logical counters for the queried
/// scope.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct VectorSlabStats {
    /// Whole-slab physical facts (always global, never scoped by `index_id`).
    pub slab: VectorSlabGlobalStats,
    /// Logical counters for the queried scope (one `index_id`, or all indexes).
    pub scope: VectorSlabScopeStats,
    /// Per-`(index_id, index_version)` breakdown for the queried scope.
    pub versions: Vec<VectorSlabVersionStats>,
}

/// Whole-slab physical facts for [`VectorSlabStats`]. Always global: the `VECTOR_ROW_SLAB` region is
/// one allocation domain shared by all indexes and versions, so these fields are identical
/// regardless of any `index_id` filter.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub struct VectorSlabGlobalStats {
    /// Total bytes backing the raw slab region (`Memory::size() * wasm_page_size`).
    pub slab_size_bytes: u64,
    /// Bytes the slab considers allocated (the slab header's `occupied_tail`).
    pub occupied_tail_bytes: u64,
    /// Sum of every referenced page's span across the whole slab (all indexes/versions).
    pub referenced_page_bytes_global: u64,
    /// Approximate leaked/dead bytes:
    /// `occupied_tail_bytes - slab_header_len - referenced_page_bytes_global`, saturating at zero.
    /// Conservative; grows as cleanup deletes page meta without rewinding the slab tail.
    ///
    /// **Meaningful only in a whole-slab result** (the unbounded `admin_vector_slab_stats`, or a
    /// client-merged set of [`VectorSlabStatsStep`]s). It is always `0` inside a per-step
    /// [`VectorSlabStatsStep::partial`], because a single bounded step has not yet observed every
    /// referenced page; the caller recomputes the estimate after merging all steps.
    pub estimated_unreferenced_bytes: u64,
}

/// One bounded page-meta scan step for the cursor/budgeted `admin_vector_slab_stats_step` query.
///
/// IC-safe incremental variant of [`VectorSlabStats`]: each call scans at most a budgeted number of
/// `VECTOR_PAGE_META` entries, then returns a `cursor` to resume from. Callers repeat until
/// `exhausted` is `true` and merge the partials client-side.
///
/// **Merge contract.** `partial` is *additive* across steps, with these exceptions:
/// - `partial.slab.slab_size_bytes` and `partial.slab.occupied_tail_bytes` are repeated physical
///   snapshots, not sums; take any step's value (the last is freshest).
/// - `partial.slab.estimated_unreferenced_bytes` is always `0` per step. The final dead-space
///   estimate is computed once after merging:
///   `occupied_tail_bytes - slab_header_len - sum(partial.slab.referenced_page_bytes_global)`,
///   saturating at zero.
/// - `partial.slab.referenced_page_bytes_global`, `partial.scope`, and `partial.versions` are summed
///   (versions by `(index_id, index_version)` key).
///
/// `partial.slab.referenced_page_bytes_global` accumulates the span of *every* page observed in the
/// step, even pages outside a `Some(index_id)` filter, because `VECTOR_ROW_SLAB` is one global
/// allocation domain. `partial.scope`/`partial.versions` only count pages within the scope.
///
/// `cursor` is opaque `PageKey` bytes; pass it back verbatim. It is `None` exactly when `exhausted`.
///
/// **No snapshot isolation.** This is a bounded *best-effort* scan, not a point-in-time snapshot. The
/// cursor is only a `PageKey`, so concurrent `VECTOR_PAGE_META` writes between steps are not isolated:
/// a page inserted *before* the cursor is missed, a page already counted then deleted still lingers in
/// the merged total, and the last step's `occupied_tail_bytes` may pair with referenced bytes
/// gathered from earlier states. For an exact whole-slab figure, either run the steps during a
/// quiescent (no-write) window or use the unbounded single-call `admin_vector_slab_stats`. Since these
/// are diagnostic-only counters, small cross-call drift is acceptable.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct VectorSlabStatsStep {
    /// This step's additive contribution (see the merge contract above).
    pub partial: VectorSlabStats,
    /// Opaque resume cursor (`PageKey` bytes); `None` exactly when `exhausted`.
    pub cursor: Option<Vec<u8>>,
    /// `true` once the whole page-meta map has been scanned.
    pub exhausted: bool,
}

/// Logical counters aggregated over the queried scope for [`VectorSlabStats`].
///
/// When `index_id` is `Some(id)` these cover only `id`; when `None` they aggregate every index.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub struct VectorSlabScopeStats {
    /// The queried index filter (`None` = all indexes).
    pub index_id: Option<u32>,
    /// Sum of referenced page spans within the scope.
    pub referenced_page_bytes: u64,
    /// Page-meta entries within the scope.
    pub page_count: u64,
    /// Physical rows (live + tombstoned) within the scope.
    pub row_count: u64,
    /// Physical non-tombstone rows (`VectorPageMeta.live_count`) within the scope. **Not**
    /// subject-freshness: ADR 0032 lets the search freshness check skip stale/meta-drift rows, so
    /// this can exceed the number of searchable rows.
    pub physical_live_row_count: u64,
    /// Tombstoned rows within the scope.
    pub tombstone_row_count: u64,
}

/// Per-`(index_id, index_version)` slab counters for [`VectorSlabStats::versions`].
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub struct VectorSlabVersionStats {
    /// Owning index.
    pub index_id: u32,
    /// Physical index generation.
    pub index_version: u64,
    /// Page-meta entries for this version.
    pub page_count: u64,
    /// Physical rows (live + tombstoned) for this version.
    pub row_count: u64,
    /// Physical non-tombstone rows for this version (see [`VectorSlabScopeStats::physical_live_row_count`]).
    pub physical_live_row_count: u64,
    /// Tombstoned rows for this version.
    pub tombstone_row_count: u64,
    /// Sum of referenced page spans for this version.
    pub referenced_page_bytes: u64,
}

/// Vector-index canister mutation/sync/admin/search failure.
///
/// Single error type for the canister: mutation endpoints return it over the wire; admin endpoints
/// map it to a `String` at the canister boundary (mirroring `graph-index`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub enum VectorIndexError {
    /// Caller is not the authorized router (admin endpoints).
    Unauthorized,
    /// The configured router principal is the anonymous principal.
    AnonymousRouter,
    /// A shard-canister principal supplied to attach is anonymous/invalid.
    InvalidPrincipalInRegistry,
    /// `shard_id` or principal is already attached to a different counterpart.
    ShardCanisterAlreadyAttached,
    /// The vector canister is already bound to a different graph (a vector target owns the whole
    /// graph, so attaching a shard of another graph is rejected).
    GraphOwnershipMismatch,
    /// Invalid index group configuration (e.g. zero group size). Retained for wire compatibility;
    /// vector attach no longer uses property-index group sharding.
    InvalidIndexGroupConfig,
    /// `shard_id` is outside the attached index group range. Retained for wire compatibility;
    /// vector attach no longer uses property-index group sharding.
    ShardOutOfRangeForGroup,
    /// Caller is not an attached graph shard for the requested `shard_id`.
    ShardNotAttached,
    /// Caller is not the attached canister for `shard_id`.
    WrongShardCanister,
    /// `subject.shard_id` does not match the caller's attached shard.
    ShardMismatch,
    /// No index definition for `index_id`.
    UnknownIndex,
    /// `encoding`/`dims` on an upsert disagree with the index definition.
    DimensionMismatch,
    /// `bytes.len()` does not equal `dims * stride` for an upsert.
    ByteWidthMismatch,
    /// A same-`embedding_version` upsert arrived with a different payload on a live subject.
    EmbeddingVersionConflict,
    /// The op's `remove` flag disagrees with the invoked mutation endpoint (e.g. `vector_upsert`
    /// received `remove = true`).
    MutationKindMismatch,
    /// An index definition whose `slots_per_page` would be `< 1`.
    InvalidPageCapacity,
    /// Internal allocator exhausted (`u64` overflow); not reachable in practice.
    AllocatorOverflow,
    /// `top_k` on a vector search is `0` or exceeds [`MAX_VECTOR_SEARCH_TOP_K`].
    InvalidSearchTopK,
    /// The supplied candidate allowlist is malformed, contains a non-vertex subject, contains
    /// duplicates, or exceeds [`MAX_VECTOR_SEARCH_FILTER_CANDIDATES`] (ADR 0034 Slice 6).
    InvalidSearchCandidates,
    /// The metric is not supported for the selected physical read path in this slice
    /// (e.g. `Cosine` with `nlist > 1` partition-page scan).
    MetricNotSupportedForPartitionScan,
    /// A sync op's metric disagrees with the stored lazy `VectorIndexDef.metric`.
    MetricMismatch,
    /// The query vector is non-finite or has zero norm where the metric requires a finite,
    /// non-zero vector.
    InvalidQueryVector,
    /// A rebuild is already in flight for the index (ADR 0031 Slice 7); abort it first.
    RebuildAlreadyActive,
    /// No rebuild is in flight for the index (step/status/publish/abort with nothing to do).
    NoActiveRebuild,
    /// Publish requested while the rebuild is not yet `ReadyToPublish`.
    RebuildNotReadyToPublish,
    /// Publish requested but completeness invariants are not satisfied (e.g. centroids missing).
    RebuildIncomplete,
    /// Invalid rebuild parameters (`nlist` / `sample_limit` out of range, wrong encoding/metric).
    InvalidRebuildParams,
    /// Stable memory `grow` failed while reserving a slab page for a row append (ADR 0032).
    StableGrowFailed,
    /// A caller-supplied `admin_vector_slab_stats_step` cursor is malformed (wrong byte length).
    InvalidStatsCursor,
    /// A maintenance policy is invalid (e.g. a `recommended_*_bps` exceeds its `required_*_bps`).
    InvalidMaintenancePolicy,
    /// Caller-attested maintenance health is stale: its `index_id`/`index_version`/`nlist` do not
    /// match the index's current active generation (ADR 0031 Slice 9 rebuild trigger).
    StaleMaintenanceHealth,
}

impl std::fmt::Display for VectorIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Unauthorized => "caller is not the authorized router",
            Self::AnonymousRouter => "router principal must not be the anonymous principal",
            Self::InvalidPrincipalInRegistry => "invalid principal in shard registry",
            Self::ShardCanisterAlreadyAttached => {
                "shard/canister attachment already exists with a different counterpart"
            }
            Self::GraphOwnershipMismatch => {
                "vector index canister is already bound to a different graph"
            }
            Self::InvalidIndexGroupConfig => "invalid index group configuration",
            Self::ShardOutOfRangeForGroup => "shard id is outside the attached index group range",
            Self::ShardNotAttached => "caller is not an attached graph shard",
            Self::WrongShardCanister => "caller is not the attached canister for this shard",
            Self::ShardMismatch => "subject shard does not match attached shard",
            Self::UnknownIndex => "unknown vector index id",
            Self::DimensionMismatch => "embedding encoding/dims disagree with the index definition",
            Self::ByteWidthMismatch => "embedding byte width does not match dims * stride",
            Self::EmbeddingVersionConflict => {
                "same embedding_version upsert with a different payload"
            }
            Self::MutationKindMismatch => {
                "sync op remove flag disagrees with the mutation endpoint"
            }
            Self::InvalidPageCapacity => "index page capacity yields fewer than one slot per page",
            Self::AllocatorOverflow => "vector index allocator overflow",
            Self::InvalidSearchTopK => "search top_k must be in 1..=MAX_VECTOR_SEARCH_TOP_K",
            Self::InvalidSearchCandidates => {
                "search candidate allowlist is malformed or exceeds the supported bound"
            }
            Self::MetricNotSupportedForPartitionScan => {
                "metric is not supported for the partition-page scan path"
            }
            Self::MetricMismatch => "sync op metric disagrees with the index definition",
            Self::InvalidQueryVector => "query vector is non-finite or has zero norm",
            Self::RebuildAlreadyActive => "a vector rebuild is already active for this index",
            Self::NoActiveRebuild => "no vector rebuild is active for this index",
            Self::RebuildNotReadyToPublish => "vector rebuild is not ready to publish",
            Self::RebuildIncomplete => "vector rebuild completeness invariants are not satisfied",
            Self::InvalidRebuildParams => "invalid vector rebuild parameters",
            Self::StableGrowFailed => "stable memory grow failed while reserving a slab page",
            Self::InvalidStatsCursor => "malformed slab stats cursor",
            Self::InvalidMaintenancePolicy => "invalid vector maintenance policy",
            Self::StaleMaintenanceHealth => {
                "attested maintenance health does not match the active index generation"
            }
        };
        f.write_str(text)
    }
}

impl std::error::Error for VectorIndexError {}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};

    #[test]
    fn encoding_stride_bytes() {
        assert_eq!(VectorEncoding::F32.component_bytes(), 4);
        assert_eq!(VectorEncoding::F32.stride_bytes(8), 32);
    }

    #[test]
    fn sync_op_candid_roundtrip() {
        let op = VectorEmbeddingSyncOp {
            index_id: 7,
            embedding_name_id: 3,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(2),
                vertex_id: 42,
            },
            embedding_incarnation: 5,
            embedding_version: 9,
            encoding: VectorEncoding::F32,
            dims: 4,
            metric: VectorMetric::L2Squared,
            bytes: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            remove: false,
        };
        let bytes = Encode!(&op).expect("encode");
        assert_eq!(Decode!(&bytes, VectorEmbeddingSyncOp).expect("decode"), op);
    }

    #[test]
    fn remove_op_carries_empty_bytes() {
        let op = VectorEmbeddingSyncOp {
            index_id: 1,
            embedding_name_id: 0,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: 1,
            },
            embedding_incarnation: 1,
            embedding_version: 2,
            encoding: VectorEncoding::F32,
            dims: 4,
            metric: VectorMetric::Cosine,
            bytes: Vec::new(),
            remove: true,
        };
        let bytes = Encode!(&op).expect("encode");
        let decoded = Decode!(&bytes, VectorEmbeddingSyncOp).expect("decode");
        assert!(decoded.remove);
        assert!(decoded.bytes.is_empty());
        assert_eq!(decoded.embedding_incarnation, 1);
    }

    #[test]
    fn catalog_lookup_and_candid_roundtrip() {
        let catalog = IndexedEmbeddingCatalog {
            embeddings: vec![IndexedEmbeddingSpec {
                embedding_name_id: 5,
                index_id: 11,
                kind: VectorIndexKind::IvfFlat,
                metric: VectorMetric::L2Squared,
                encoding: VectorEncoding::F32,
                dims: 16,
            }],
        };
        assert!(!catalog.is_empty());
        assert_eq!(catalog.spec_for(5).expect("spec").index_id, 11);
        assert!(catalog.spec_for(6).is_none());
        let bytes = Encode!(&catalog).expect("encode");
        assert_eq!(
            Decode!(&bytes, IndexedEmbeddingCatalog).expect("decode"),
            catalog
        );
        assert!(IndexedEmbeddingCatalog::default().is_empty());
    }

    #[test]
    fn error_candid_roundtrip() {
        for err in [
            VectorIndexError::EmbeddingVersionConflict,
            VectorIndexError::InvalidSearchTopK,
            VectorIndexError::StableGrowFailed,
            VectorIndexError::InvalidStatsCursor,
            VectorIndexError::InvalidMaintenancePolicy,
            VectorIndexError::StaleMaintenanceHealth,
        ] {
            let bytes = Encode!(&err).expect("encode");
            assert_eq!(Decode!(&bytes, VectorIndexError).expect("decode"), err);
        }
    }

    #[test]
    fn search_request_and_result_candid_roundtrip() {
        let req = VectorSearchRequest {
            index_id: 7,
            query: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            encoding: VectorEncoding::F32,
            dims: 4,
            metric: VectorMetric::L2Squared,
            top_k: 10,
            candidate_subjects: None,
        };
        let bytes = Encode!(&req).expect("encode request");
        assert_eq!(Decode!(&bytes, VectorSearchRequest).expect("decode"), req);

        let req_with_candidates = VectorSearchRequest {
            index_id: 7,
            query: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            encoding: VectorEncoding::F32,
            dims: 4,
            metric: VectorMetric::L2Squared,
            top_k: 10,
            candidate_subjects: Some(vec![VectorSubject::Vertex {
                shard_id: ShardId::new(2),
                vertex_id: 42,
            }]),
        };
        let bytes = Encode!(&req_with_candidates).expect("encode request with candidates");
        assert_eq!(
            Decode!(&bytes, VectorSearchRequest).expect("decode with candidates"),
            req_with_candidates
        );

        let empty_candidates = VectorSearchRequest {
            index_id: 7,
            query: vec![],
            encoding: VectorEncoding::F32,
            dims: 4,
            metric: VectorMetric::L2Squared,
            top_k: 10,
            candidate_subjects: Some(vec![]),
        };
        let bytes = Encode!(&empty_candidates).expect("encode empty candidates");
        assert_eq!(
            Decode!(&bytes, VectorSearchRequest).expect("decode empty candidates"),
            empty_candidates
        );

        let result = VectorSearchResult {
            hits: vec![VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(2),
                    vertex_id: 42,
                },
                distance: 1.5,
                embedding_incarnation: 3,
                embedding_version: 9,
            }],
        };
        let bytes = Encode!(&result).expect("encode result");
        assert_eq!(Decode!(&bytes, VectorSearchResult).expect("decode"), result);
    }

    #[test]
    fn cosine_search_request_and_result_candid_roundtrip() {
        let req = VectorSearchRequest {
            index_id: 7,
            query: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            encoding: VectorEncoding::F32,
            dims: 4,
            metric: VectorMetric::Cosine,
            top_k: 10,
            candidate_subjects: None,
        };
        let bytes = Encode!(&req).expect("encode request");
        assert_eq!(Decode!(&bytes, VectorSearchRequest).expect("decode"), req);

        let req_with_candidates = VectorSearchRequest {
            index_id: 7,
            query: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            encoding: VectorEncoding::F32,
            dims: 4,
            metric: VectorMetric::L2Squared,
            top_k: 10,
            candidate_subjects: Some(vec![VectorSubject::Vertex {
                shard_id: ShardId::new(2),
                vertex_id: 42,
            }]),
        };
        let bytes = Encode!(&req_with_candidates).expect("encode request with candidates");
        assert_eq!(
            Decode!(&bytes, VectorSearchRequest).expect("decode with candidates"),
            req_with_candidates
        );

        let empty_candidates = VectorSearchRequest {
            index_id: 7,
            query: vec![],
            encoding: VectorEncoding::F32,
            dims: 4,
            metric: VectorMetric::L2Squared,
            top_k: 10,
            candidate_subjects: Some(vec![]),
        };
        let bytes = Encode!(&empty_candidates).expect("encode empty candidates");
        assert_eq!(
            Decode!(&bytes, VectorSearchRequest).expect("decode empty candidates"),
            empty_candidates
        );

        let result = VectorSearchResult {
            hits: vec![VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(2),
                    vertex_id: 42,
                },
                distance: 0.25,
                embedding_incarnation: 3,
                embedding_version: 9,
            }],
        };
        let bytes = Encode!(&result).expect("encode result");
        assert_eq!(Decode!(&bytes, VectorSearchResult).expect("decode"), result);
    }

    #[test]
    fn vector_metric_output_shape() {
        assert_eq!(
            VectorMetric::L2Squared.output_shape(),
            VectorOutputShape::Distance
        );
        assert_eq!(
            VectorMetric::Cosine.output_shape(),
            VectorOutputShape::Score
        );
    }

    #[test]
    fn l2_squared_user_scalar_conversion() {
        assert_eq!(VectorMetric::L2Squared.to_user_distance(2.5), Some(2.5));
        assert!(VectorMetric::L2Squared.to_user_score(2.5).is_none());
        assert!(VectorMetric::L2Squared.to_user_distance(f32::NAN).is_none());
        assert!(
            VectorMetric::L2Squared
                .to_user_distance(f32::INFINITY)
                .is_none()
        );
        assert!(
            VectorMetric::L2Squared
                .to_user_distance(f32::NEG_INFINITY)
                .is_none()
        );
    }

    #[test]
    fn cosine_user_scalar_conversion() {
        // raw = 1 - similarity; score = 1 - raw = similarity.
        assert_eq!(VectorMetric::Cosine.to_user_score(0.25), Some(0.75));
        assert!(VectorMetric::Cosine.to_user_distance(0.25).is_none());
        assert!(VectorMetric::Cosine.to_user_score(f32::NAN).is_none());
        assert!(VectorMetric::Cosine.to_user_score(f32::INFINITY).is_none());
    }

    #[test]
    fn new_vector_index_errors_candid_roundtrip() {
        for err in [
            VectorIndexError::MetricNotSupportedForPartitionScan,
            VectorIndexError::MetricMismatch,
            VectorIndexError::InvalidQueryVector,
            VectorIndexError::InvalidSearchCandidates,
        ] {
            let bytes = Encode!(&err).expect("encode");
            assert_eq!(Decode!(&bytes, VectorIndexError).expect("decode"), err);
        }
    }

    #[test]
    fn slab_stats_candid_roundtrip() {
        let stats = VectorSlabStats {
            slab: VectorSlabGlobalStats {
                slab_size_bytes: 131_072,
                occupied_tail_bytes: 96_000,
                referenced_page_bytes_global: 64_000,
                estimated_unreferenced_bytes: 31_968,
            },
            scope: VectorSlabScopeStats {
                index_id: Some(7),
                referenced_page_bytes: 48_000,
                page_count: 3,
                row_count: 120,
                physical_live_row_count: 100,
                tombstone_row_count: 20,
            },
            versions: vec![
                VectorSlabVersionStats {
                    index_id: 7,
                    index_version: 1,
                    page_count: 2,
                    row_count: 80,
                    physical_live_row_count: 70,
                    tombstone_row_count: 10,
                    referenced_page_bytes: 32_000,
                },
                VectorSlabVersionStats {
                    index_id: 7,
                    index_version: 2,
                    page_count: 1,
                    row_count: 40,
                    physical_live_row_count: 30,
                    tombstone_row_count: 10,
                    referenced_page_bytes: 16_000,
                },
            ],
        };
        let bytes = Encode!(&stats).expect("encode");
        assert_eq!(Decode!(&bytes, VectorSlabStats).expect("decode"), stats);
    }

    #[test]
    fn partition_health_step_candid_roundtrip() {
        let step = VectorPartitionHealthStep {
            partial: VectorPartitionPageHealth {
                index_id: 7,
                index_version: 3,
                page_count: 12,
                total_rows: 400,
                physical_live_rows: 320,
                tombstoned_rows: 80,
            },
            cursor: Some(vec![0u8; 24]),
            exhausted: false,
        };
        let bytes = Encode!(&step).expect("encode");
        assert_eq!(
            Decode!(&bytes, VectorPartitionHealthStep).expect("decode"),
            step
        );
    }

    #[test]
    fn maintenance_policy_and_recommendation_candid_roundtrip() {
        let policy = VectorMaintenancePolicy {
            recommended_tombstone_ratio_bps: 2_000,
            required_tombstone_ratio_bps: 5_000,
            recommended_skew_ratio_bps: 20_000,
            required_skew_ratio_bps: 40_000,
            min_total_rows: 1_000,
            min_tombstoned_rows: 100,
        };
        let bytes = Encode!(&policy).expect("encode");
        assert_eq!(
            Decode!(&bytes, VectorMaintenancePolicy).expect("decode"),
            policy
        );

        for rec in [
            VectorMaintenanceRecommendation::Healthy,
            VectorMaintenanceRecommendation::RebuildRecommended,
            VectorMaintenanceRecommendation::RebuildRequired,
        ] {
            let bytes = Encode!(&rec).expect("encode");
            assert_eq!(
                Decode!(&bytes, VectorMaintenanceRecommendation).expect("decode"),
                rec
            );
        }
    }

    #[test]
    fn maintenance_step_wire_candid_roundtrip() {
        let req = VectorMaintenanceStepRequest {
            policy: VectorMaintenancePolicy {
                recommended_tombstone_ratio_bps: 2_000,
                required_tombstone_ratio_bps: 5_000,
                recommended_skew_ratio_bps: 20_000,
                required_skew_ratio_bps: 40_000,
                min_total_rows: 1_000,
                min_tombstoned_rows: 100,
            },
            target_nlist: Some(8),
            sample_limit: 10_000,
            scan_max_pages: 64,
            rebuild_max_subjects: 5_000,
            cleanup_max_work: 5_000,
        };
        let bytes = Encode!(&req).expect("encode");
        assert_eq!(
            Decode!(&bytes, VectorMaintenanceStepRequest).expect("decode"),
            req
        );

        let page = VectorPartitionPageHealth {
            index_id: 1,
            index_version: 3,
            page_count: 4,
            total_rows: 1_000,
            physical_live_rows: 700,
            tombstoned_rows: 300,
        };
        let states = [
            VectorMaintenanceState::Idle,
            VectorMaintenanceState::Scanning {
                cursor: Some(vec![1, 2, 3]),
                exhausted: false,
                merged: page,
            },
            VectorMaintenanceState::Scanning {
                cursor: None,
                exhausted: true,
                merged: page,
            },
            VectorMaintenanceState::Failed(VectorMaintenanceFailure {
                code: VectorIndexError::RebuildAlreadyActive,
                message: "boom".to_string(),
            }),
        ];
        for state in states {
            let bytes = Encode!(&state).expect("encode");
            assert_eq!(
                Decode!(&bytes, VectorMaintenanceState).expect("decode"),
                state
            );
        }

        let status = VectorRebuildStatus {
            phase: VectorRebuildPhase::Building,
            target_index_version: 4,
            nlist: 8,
            subjects_processed: 12,
            candidates_collected: 0,
            training_iteration: 0,
        };
        let results = [
            VectorMaintenanceStepResult::Scanning { exhausted: true },
            VectorMaintenanceStepResult::Healthy,
            VectorMaintenanceStepResult::RebuildStarted(
                VectorMaintenanceRecommendation::RebuildRequired,
            ),
            VectorMaintenanceStepResult::RebuildAdvanced(status.clone()),
            VectorMaintenanceStepResult::AwaitingPublish(status.clone()),
            VectorMaintenanceStepResult::CleanupAdvanced(status),
            VectorMaintenanceStepResult::Failed(VectorMaintenanceFailure {
                code: VectorIndexError::InvalidRebuildParams,
                message: "nope".to_string(),
            }),
        ];
        for result in results {
            let bytes = Encode!(&result).expect("encode");
            assert_eq!(
                Decode!(&bytes, VectorMaintenanceStepResult).expect("decode"),
                result
            );
        }
    }

    #[test]
    fn centroid_cache_status_candid_roundtrip() {
        let status = VectorCentroidCacheStatus {
            entries: 3,
            bytes: 49_152,
            max_bytes: 1_048_576,
        };
        let bytes = Encode!(&status).expect("encode");
        assert_eq!(
            Decode!(&bytes, VectorCentroidCacheStatus).expect("decode"),
            status
        );
    }

    #[test]
    fn subject_shard_accessor() {
        let subject = VectorSubject::Vertex {
            shard_id: ShardId::new(4),
            vertex_id: 9,
        };
        assert_eq!(subject.shard_id(), ShardId::new(4));
    }
}
