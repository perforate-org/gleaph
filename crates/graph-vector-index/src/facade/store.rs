//! Stateless facade over stable vector-index storage ([`super::stable`]).
//!
//! Storage domains (ADR 0031 Slice 2):
//! - [`authorization`] — router auth, shard-canister attachments, detach purge
//! - [`mutation`] — `vector_upsert` / `vector_remove` with embedding_version + subject-clock
//!   idempotence over a degenerate `ivf_flat` page store

mod authorization;
mod centroid_cache;
mod maintenance;
mod maintenance_step;
mod mutation;
mod rebuild;
mod search;

pub(crate) use maintenance::recommend_partition_maintenance;

#[cfg(feature = "canbench")]
pub(crate) use search::SearchTuning;

#[cfg(any(test, feature = "canbench"))]
mod seed;

#[cfg(test)]
mod tests;

/// Default page byte budget when an index def is created lazily on first upsert.
///
/// Chosen for one StableMemory-friendly read plus a heap scoring buffer once search lands (Slice
/// 4+). `slots_per_page` is derived from this and the index `stride_bytes` (see [`mutation`]).
pub(crate) const DEFAULT_MAX_PAGE_BYTES: u32 = 64 * 1024;

/// Fixed per-page overhead reserved for the page header when computing `slots_per_page`.
pub(crate) const PAGE_HEADER_BYTES: u32 = 64;

/// Degenerate `ivf_flat` partition: a single partition `0` in Slice 2.
pub(crate) const DEGENERATE_PARTITION_ID: u32 = 0;

/// First physical index generation, assigned on index creation.
pub(crate) const INITIAL_INDEX_VERSION: u64 = 1;

/// First `VectorId` / `generation`; `0` is reserved as "none".
pub(crate) const FIRST_ALLOCATION: u64 = 1;

/// Upper bound on a production rebuild's `nlist` (ADR 0031 Slice 7). Bounds the centroid/head counts
/// and the durable `Sampling.candidates` vector so worst-case rebuild-state bytes
/// (`MAX_NLIST * stride_bytes`) and the O(`nlist`) teardown reads/deletes stay within budget.
pub(crate) const MAX_NLIST: u32 = 1024;

/// Upper bound on the number of live subjects a rebuild's `Sampling` phase will examine while
/// collecting centroid candidates (ADR 0031 Slice 7). Bounds the total sampling work.
pub(crate) const MAX_REBUILD_SAMPLE_LIMIT: u32 = 1_000_000;

/// Canister-side ceiling on the per-step work (`max_subjects` / `max_work`) any rebuild step or
/// cleanup step will perform in one message (ADR 0031 Slice 7). The caller-supplied budget is
/// clamped to `1..=MAX_REBUILD_STEP_WORK` so a Router that passes a huge value (e.g. `u32::MAX`)
/// still cannot force an O(N) scan/drop in a single message. Mirrors
/// `MAX_DETACH_EXAMINE_PER_STEP`'s bounded-step precedent.
pub(crate) const MAX_REBUILD_STEP_WORK: u32 = 20_000;

/// Canister-side ceiling on the transient vector bytes a single `Sampling`/`Building` step buffers
/// on the heap before processing (ADR 0031 Slice 7). The row-count cap [`MAX_REBUILD_STEP_WORK`]
/// alone does not bound heap use because each buffered vector is `stride_bytes` wide and
/// `stride_bytes` scales with `dims`; a step therefore also breaks once cumulative read bytes reach
/// this budget (always processing at least one row first, so forward progress is guaranteed even
/// when a single vector exceeds the budget).
pub(crate) const MAX_REBUILD_STEP_VECTOR_BYTES: u64 = 8 * 1024 * 1024;

/// Upper bound on the Candid-encoded `VectorRebuildStateRecord` value, i.e. the combined durable
/// rebuild-state envelope (ADR 0031 Slice 7/8). This is an encoded `to_bytes().len()` cap (it
/// accounts for enum/vec-length/nested-vec overhead), not a raw-vector-bytes cap. The `Training`
/// value holds both the candidate pool and the trained centroids, so the candidate pool is sized to
/// reserve `nlist * stride_bytes` (centroids) plus [`MAX_REBUILD_STATE_OVERHEAD_BYTES`] inside this
/// envelope; the sampling->`Training` transition additionally re-checks the encoded length and
/// fails closed (`InvalidRebuildParams`) rather than trapping if it would exceed the cap.
pub(crate) const MAX_REBUILD_STATE_BYTES: u64 = 8 * 1024 * 1024;

/// Conservative reserve subtracted from [`MAX_REBUILD_STATE_BYTES`] when sizing the candidate pool,
/// absorbing the Candid enum tag / vec-length / nested-vec encoding overhead so the encoded
/// `Training` value stays within the envelope at the boundary (ADR 0031 Slice 8).
pub(crate) const MAX_REBUILD_STATE_OVERHEAD_BYTES: u64 = 64 * 1024;

/// Maximum k-means-lite iterations a `Training` phase performs before writing centroids and
/// transitioning to `Building` (ADR 0031 Slice 8). Each iteration is one bounded `*_step` message.
pub(crate) const MAX_REBUILD_TRAINING_ITERATIONS: u32 = 8;

/// Per-iteration distance-op ceiling for `Training`: the candidate pool is sized so one full
/// k-means-lite iteration's `candidate_count * nlist * dims` distance computations never exceed this
/// budget (ADR 0031 Slice 8). Chosen large enough that any `nlist`/`dims` admitted by
/// [`MAX_REBUILD_STATE_BYTES`] can still sample `>= nlist` candidates (so the op check is a
/// defensive feasibility guard the state cap normally subsumes), yet small enough that one iteration
/// stays within the per-message instruction budget.
pub(crate) const MAX_REBUILD_TRAINING_DISTANCE_OPS: u64 = 1_100_000_000;

/// Stateless facade over vector-index stable structures initialized in [`super::stable`].
#[derive(Clone, Copy, Debug, Default)]
pub struct VectorIndexStore;

impl VectorIndexStore {
    pub const fn new() -> Self {
        Self
    }
}
