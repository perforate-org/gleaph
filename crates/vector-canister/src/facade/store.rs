//! Stateless facade over stable vector-index storage ([`super::stable`]).
//!
//! Storage domains (ADR 0031 Slice 2):
//! - [`authorization`] — router auth, shard-canister attachments, detach purge
//! - [`mutation`] — `vector_upsert` / `vector_remove` with embedding_version + subject-clock
//!   idempotence over a degenerate `ivf_flat` page store
//!
//! Every facade entry point is a free function over the stable-memory statics owned by
//! [`super::stable`]; there is no instance layer.

mod authorization;
mod centroid_cache;
mod compact;
mod maintenance;
mod maintenance_step;
mod mutation;
mod rebuild;
mod search;
#[cfg(test)]
pub(crate) use search::page_skip_stats;
#[cfg(any(test, feature = "canbench"))]
pub(crate) use search::reset_page_skip_stats;
mod watermark;

use crate::facade::stable::region_store::RegionError;
use gleaph_graph_kernel::vector_index::VectorCanisterError;

pub(crate) use maintenance::recommend_partition_maintenance;
pub(crate) use watermark::{advance_watermark, gc_subjects_step};

#[cfg(feature = "canbench")]
pub(crate) use search::SearchTuning;

#[cfg(any(test, feature = "canbench"))]
mod seed;

#[cfg(test)]
mod tests;

/// Default page byte budget when an index def is created lazily on first upsert.
///
/// `slots_per_page` is derived from this and the index geometry (pad stride, meta stride, run
/// capacity) via the page-store budget solver (see [`mutation`]).
pub(crate) const DEFAULT_MAX_PAGE_BYTES: u32 = 64 * 1024;

/// Degenerate `ivf_flat` partition: a single partition `0` in Slice 2.
pub(crate) const DEGENERATE_PARTITION_ID: u32 = 0;

/// First physical index generation, assigned on index creation.
pub(crate) const INITIAL_INDEX_VERSION: u64 = 1;

/// Upper bound on a production rebuild's `nlist` (ADR 0031 Slice 7). Bounds the centroid/head counts
/// and the durable `Sampling.candidates` vector so worst-case rebuild-state bytes
/// (`MAX_NLIST * stride_bytes`) and the O(`nlist`) teardown reads/deletes stay within budget.
/// Applied **per level** for a two-level rebuild: the coarse count is capped by `MAX_NLIST` and
/// the total leaf count by [`MAX_LEAVES`].
pub(crate) const MAX_NLIST: u32 = 1024;

/// Upper bound on the total leaf count (`nlist * nlist_fine`) of a two-level index generation
/// (Slice 5). Grounds: leaf ids pack into `u32` partition-id space as `coarse * f + fine`
/// (`65,536²` leaves would overflow practical head/page addressing budgets), and heads/pages are
/// materialized per leaf, so the bound caps both the key space and head capacity per generation.
pub(crate) const MAX_LEAVES: u32 = 65_536;

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

/// Per-message ceiling on directory entries one slab-compaction step examines (mirrors
/// `MAX_SLAB_STATS_STEP_PAGES`). The caller-supplied budget is clamped to `1..=` this cap.
pub(crate) const MAX_COMPACT_STEP_PAGES: u32 = 20_000;

/// Per-message ceiling on page bytes one slab-compaction step copies down (mirrors
/// [`MAX_REBUILD_STEP_VECTOR_BYTES`]). The first in-range page is always admitted regardless of
/// this budget, so every step makes forward progress even for pages larger than the cap.
pub(crate) const MAX_COMPACT_STEP_BYTES: u64 = 8 * 1024 * 1024;

/// Maximum k-means-lite iterations a `Training` phase performs before writing centroids and
/// transitioning to `Building` (ADR 0031 Slice 8). Each iteration is one bounded `*_step` message.
pub(crate) const MAX_REBUILD_TRAINING_ITERATIONS: u32 = 8;

/// Per-iteration distance-op ceiling for `Training`: the candidate pool is sized so one full
/// k-means-lite iteration's `candidate_count * nlist * dims` distance computations never exceed this
/// budget (ADR 0031 Slice 8). Chosen large enough that any geometry admitted by the rebuild-pool
/// region budget (`rebuild_pool::REGION_BYTES`) can still sample `>= nlist` candidates (so the op
/// check is a defensive feasibility guard the region budget normally subsumes), yet small enough
/// that one iteration stays within the per-message instruction budget.
pub(crate) const MAX_REBUILD_TRAINING_DISTANCE_OPS: u64 = 1_100_000_000;

#[cfg(any(test, feature = "canbench"))]
pub(crate) use authorization::reset_for_test_or_bench;
/// Facade entry points, re-exported so callers reach every former method as a plain free
/// function from one place.
pub(crate) use authorization::{
    admin_attach_shard_canister, admin_detach_shard_canister, init_from_args,
    open_definition_store_after_upgrade, open_subject_store_after_upgrade,
};
// The flat-only start signature survives for the flat lifecycle regression tests and benches;
// the wire surface goes through `admin_start_vector_rebuild_with_fine` (Slice 5).
#[cfg(test)]
pub(crate) use authorization::{attach_single_shard_for_test, detach_shard_step_for_test};
#[cfg(feature = "canbench")]
pub(crate) use centroid_cache::warm_index;
pub(crate) use centroid_cache::{admin_vector_centroid_cache_status, warm_all};
pub(crate) use compact::{
    admin_start_vector_slab_compact, admin_vector_slab_compact_status,
    admin_vector_slab_compact_step,
};
pub(crate) use maintenance_step::{
    admin_vector_maintenance_reset, admin_vector_maintenance_status, admin_vector_maintenance_step,
};
#[cfg(test)]
pub(crate) use mutation::{
    create_index_for_test, def_for_test, partition_head_for_test, subject_entry_for_test,
};
pub(crate) use mutation::{
    preflight_vector_sync_batch, vector_remove, vector_sync_batch_outcome_chunk, vector_upsert,
};
#[cfg(any(test, feature = "canbench"))]
pub(crate) use rebuild::admin_start_vector_rebuild;
pub(crate) use rebuild::{
    admin_abort_vector_rebuild, admin_publish_vector_rebuild,
    admin_start_vector_rebuild_if_recommended, admin_start_vector_rebuild_with_fine,
    admin_vector_partition_health, admin_vector_partition_health_step,
    admin_vector_rebuild_cleanup_step, admin_vector_rebuild_status, admin_vector_rebuild_step,
    admin_vector_slab_stats, admin_vector_slab_stats_step,
};
pub(crate) use search::vector_search;
#[cfg(any(test, feature = "canbench"))]
pub(crate) use search::vector_search_tuned;
#[cfg(any(test, feature = "canbench"))]
pub(crate) use seed::{seed_ivf_for_test, seed_ivf_with_metric_for_test};
#[cfg(feature = "pocket-ic-e2e")]
pub(crate) use watermark::VectorTombstoneFrontierProbe;
pub(crate) use watermark::advance_router_frontier;
#[cfg(feature = "pocket-ic-e2e")]
pub(crate) use watermark::test_vector_frontier_probe;

/// Result of admitting one operation through the typed batch path.
///
/// `TablePressure` is deliberately the only terminal item result.  A definition-store failure
/// that is not pressure cannot be acknowledged by the wire outcome because that outcome carries
/// only a committed prefix; the canister handler either returns the outer availability marker
/// before the first write or traps so the IC rolls the whole message back.
#[derive(Debug)]
pub(crate) enum VectorSyncBatchOutcomeOperationError {
    TablePressure,
    SubjectTablePressure,
    StoreUnavailable,
    SubjectStoreUnavailable,
    Fatal(VectorCanisterError),
}

/// The legacy Vector wire surface has no availability or terminal-admission envelope: every
/// stable-region failure projects to the existing generic stable-write error, once, at this
/// boundary. The typed batch endpoint consumes [`RegionError`] directly and owns the richer
/// per-store classification.
impl From<RegionError> for VectorCanisterError {
    fn from(_error: RegionError) -> Self {
        VectorCanisterError::StableGrowFailed
    }
}
