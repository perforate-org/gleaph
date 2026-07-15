//! Federated derived vector-index canister (`gleaph-graph-vector-index`).
//!
//! Owns the derived `ivf_flat` search structures rebuildable from the graph canonical
//! `VertexEmbeddingStore` (ADR 0031). Slice 2 is mutation-only: `vector_upsert` / `vector_remove`
//! over a degenerate `ivf_flat` page store (`nlist = 1`, `partition_id = 0`, no centroids, no
//! search). Shard/canister attachments are configured by the router via `admin_attach_shard_canister`.
//!
//! ## API visibility
//!
//! Admin APIs are router-only (`guard_router_canister`). Mutation updates authorize the caller
//! against the shard catalog inside the store and return [`VectorIndexError`] over the wire.

mod facade;
mod records;

#[cfg(feature = "canbench")]
mod bench;

pub mod init;
pub mod state;

mod canister;
mod guards;

pub use facade::VectorIndexStore;
pub use init::VectorIndexInitArgs;
pub use state::VectorIndexError;

use crate::guards::guard_router_canister;
use candid::{CandidType, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ShardDetachCursor, ShardDetachStepResult, ShardId};
use gleaph_graph_kernel::vector_index::{
    VectorCentroidCacheStatus, VectorEmbeddingSyncOp, VectorMaintenancePolicy,
    VectorMaintenanceRecommendation, VectorMaintenanceState, VectorMaintenanceStepRequest,
    VectorMaintenanceStepResult, VectorPartitionHealthStep, VectorPartitionHealthSummary,
    VectorPartitionPageHealth, VectorRebuildStatus, VectorSearchRequest, VectorSearchResult,
    VectorSlabStats, VectorSlabStatsStep, VectorSyncBatchProgress,
};
use ic_cdk_macros::{init, query, update};

fn bounded_response<T: CandidType>(method: &str, value: T) -> T {
    let bytes = Encode!(&value).unwrap_or_else(|error| {
        ic_cdk::trap(format!("{method} response encode failed: {error}"));
    });
    if bytes.len() > gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        ic_cdk::trap(format!(
            "{method} response exceeds the safe payload limit of {} bytes",
            gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
        ));
    }
    value
}

#[init]
fn init(args: VectorIndexInitArgs) {
    canister::init(args);
}

#[update(guard = "guard_router_canister")]
fn admin_attach_shard_canister(
    graph_id: GraphId,
    shard_id: ShardId,
    shard_canister_principal: Principal,
) -> Result<(), String> {
    canister::admin_attach_shard_canister(graph_id, shard_id, shard_canister_principal)
}

#[update(guard = "guard_router_canister")]
fn admin_detach_shard_canister(
    shard_id: ShardId,
    resume: Option<ShardDetachCursor>,
) -> Result<ShardDetachStepResult, String> {
    canister::admin_detach_shard_canister(shard_id, resume)
}

#[update]
fn vector_upsert(op: VectorEmbeddingSyncOp) -> Result<(), VectorIndexError> {
    let request_bytes = Encode!(&(&op,)).expect("vector_upsert request encoding");
    if request_bytes.len() > gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        ic_cdk::trap("vector_upsert request exceeds the safe payload limit");
    }
    bounded_response("vector_upsert", canister::vector_upsert(op))
}

#[update]
fn vector_remove(op: VectorEmbeddingSyncOp) -> Result<(), VectorIndexError> {
    let request_bytes = Encode!(&(&op,)).expect("vector_remove request encoding");
    if request_bytes.len() > gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        ic_cdk::trap("vector_remove request exceeds the safe payload limit");
    }
    bounded_response("vector_remove", canister::vector_remove(op))
}

#[update]
fn vector_sync_batch(operations: Vec<VectorEmbeddingSyncOp>) -> VectorSyncBatchProgress {
    let request_bytes = Encode!(&(&operations,)).expect("vector_sync_batch request encoding");
    if request_bytes.len() > gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        ic_cdk::trap("vector_sync_batch request exceeds the safe payload limit");
    }
    match canister::vector_sync_batch(operations) {
        Ok(progress) => bounded_response("vector_sync_batch", progress),
        Err(error) => ic_cdk::trap(error.to_string()),
    }
}

/// Read-only exact `ivf_flat` top-k search (ADR 0031 Slice 5). Router-guarded like the
/// property-index reads so derived vectors cannot be queried directly, bypassing the Slice 4
/// activation/readiness gate; the Router is the activation-gated public surface.
#[query(guard = "guard_router_canister")]
fn vector_search(req: VectorSearchRequest) -> Result<VectorSearchResult, VectorIndexError> {
    bounded_response("vector_search", canister::vector_search(req))
}

/// Begins a production shadow-version rebuild for an `nlist > 1` index (ADR 0031 Slice 7). O(1);
/// the Router drives the subsequent bounded steps. Router-guarded like the other admin endpoints.
#[update(guard = "guard_router_canister")]
fn admin_start_vector_rebuild(index_id: u32, nlist: u32, sample_limit: u32) -> Result<(), String> {
    canister::admin_start_vector_rebuild(index_id, nlist, sample_limit)
}

/// Starts a rebuild only if partition health crosses the supplied policy (ADR 0031 Slice 9). The
/// head-only skew summary is recomputed server-side from current partition heads; the operator passes
/// back only the page-meta tombstone health (run to `exhausted`) with a policy. This re-derives the
/// recommendation and, when not `Healthy`, begins a rebuild (no autonomous timer). `target_nlist =
/// None` defaults to `def.nlist` only when it is `>= 2`. Returns the decided recommendation;
/// page health attested against a stale generation is rejected. Page-meta completeness is trusted
/// admin input, not proven server-side. Router-guarded.
#[update(guard = "guard_router_canister")]
fn admin_start_vector_rebuild_if_recommended(
    index_id: u32,
    attested_page_health: VectorPartitionPageHealth,
    policy: VectorMaintenancePolicy,
    target_nlist: Option<u32>,
    sample_limit: u32,
) -> Result<VectorMaintenanceRecommendation, String> {
    canister::admin_start_vector_rebuild_if_recommended(
        index_id,
        attested_page_health,
        policy,
        target_nlist,
        sample_limit,
    )
}

/// Drives one bounded `Sampling`/`Building` step of an in-flight rebuild.
#[update(guard = "guard_router_canister")]
fn admin_vector_rebuild_step(
    index_id: u32,
    max_subjects: u32,
) -> Result<VectorRebuildStatus, String> {
    canister::admin_vector_rebuild_step(index_id, max_subjects)
}

/// Reports the O(1) scalar status of a rebuild.
#[query(guard = "guard_router_canister")]
fn admin_vector_rebuild_status(index_id: u32) -> Result<VectorRebuildStatus, String> {
    canister::admin_vector_rebuild_status(index_id)
}

/// Atomically publishes a `ReadyToPublish` rebuild (O(1) def + centroid metadata flip).
#[update(guard = "guard_router_canister")]
fn admin_publish_vector_rebuild(index_id: u32) -> Result<(), String> {
    canister::admin_publish_vector_rebuild(index_id)
}

/// Aborts an in-flight rebuild; bounded teardown follows via the cleanup step.
#[update(guard = "guard_router_canister")]
fn admin_abort_vector_rebuild(index_id: u32) -> Result<(), String> {
    canister::admin_abort_vector_rebuild(index_id)
}

/// Drives one bounded teardown step of a post-publish `Cleaning` or an `Aborting` rebuild.
#[update(guard = "guard_router_canister")]
fn admin_vector_rebuild_cleanup_step(
    index_id: u32,
    max_work: u32,
) -> Result<VectorRebuildStatus, String> {
    canister::admin_vector_rebuild_cleanup_step(index_id, max_work)
}

/// Head-only O(`nlist`) partition-health summary for the active index version (ADR 0031 Slice 8).
#[query(guard = "guard_router_canister")]
fn admin_vector_partition_health(index_id: u32) -> Result<VectorPartitionHealthSummary, String> {
    canister::admin_vector_partition_health(index_id)
}

/// Bounded page-meta tombstone-health step for the active index version (ADR 0031 Slice 9). Repeat
/// with the returned `cursor` until `exhausted`, then sum the additive partials client-side (see
/// `VectorPartitionHealthStep`). Complements the head-only `admin_vector_partition_health` skew
/// summary. `max_pages` is clamped server-side; a malformed/wrong-scope `cursor` returns an error
/// rather than trapping. Diagnostic only, not search truth.
#[query(guard = "guard_router_canister")]
fn admin_vector_partition_health_step(
    index_id: u32,
    cursor: Option<Vec<u8>>,
    max_pages: u32,
) -> Result<VectorPartitionHealthStep, String> {
    canister::admin_vector_partition_health_step(index_id, cursor, max_pages)
}

/// Warms the heap centroid cache for an index from its active centroid set (ADR 0031 Slice 9). An
/// `#[update]` because a `#[query]` cannot persist heap writes on IC. Only ready `nlist > 1` indexes
/// are cached; degenerate/untrained indexes instead drop any stale entry. Returns cache status.
#[update(guard = "guard_router_canister")]
fn admin_vector_centroid_cache_warmup(index_id: u32) -> Result<VectorCentroidCacheStatus, String> {
    canister::admin_vector_centroid_cache_warmup(index_id)
}

/// Clears the entire heap centroid cache (ADR 0031 Slice 9). Router-guarded `#[update]`.
#[update(guard = "guard_router_canister")]
fn admin_vector_centroid_cache_clear() -> Result<VectorCentroidCacheStatus, String> {
    canister::admin_vector_centroid_cache_clear()
}

/// Reports heap centroid cache status (entries/bytes/cap) (ADR 0031 Slice 9). Per-query hit/miss is
/// not tracked (queries cannot commit counters on IC). Router-guarded `#[query]`.
#[query(guard = "guard_router_canister")]
fn admin_vector_centroid_cache_status() -> Result<VectorCentroidCacheStatus, String> {
    canister::admin_vector_centroid_cache_status()
}

/// Derived slab-space observability for the ADR 0032 page store. Maintenance/diagnostic data only,
/// not search truth; an unbounded full page-meta scan. `index_id` (`null` = all indexes) scopes only
/// the logical counters; the `slab` physical facts are always whole-slab global.
#[query(guard = "guard_router_canister")]
fn admin_vector_slab_stats(index_id: Option<u32>) -> Result<VectorSlabStats, String> {
    canister::admin_vector_slab_stats(index_id)
}

/// IC-safe, cursor/budgeted variant of `admin_vector_slab_stats` for large stores: one bounded
/// page-meta scan step. Repeat with the returned `cursor` until `exhausted`, then merge the additive
/// partials client-side (see `VectorSlabStatsStep`). `max_pages` is clamped server-side; a malformed
/// `cursor` returns an error rather than trapping. Diagnostic only, not search truth.
#[query(guard = "guard_router_canister")]
fn admin_vector_slab_stats_step(
    cursor: Option<Vec<u8>>,
    max_pages: u32,
    index_id: Option<u32>,
) -> Result<VectorSlabStatsStep, String> {
    canister::admin_vector_slab_stats_step(cursor, max_pages, index_id)
}

/// Advances one bounded unit of Router-forwarded vector maintenance (ADR 0031 Slice 10). The Router
/// snapshots its policy + per-step budgets into `req`; this performs at most one scan/rebuild/cleanup
/// step and stops at `ReadyToPublish` (publish stays explicit). Router-guarded `#[update]`.
#[update(guard = "guard_router_canister")]
fn admin_vector_maintenance_step(
    index_id: u32,
    req: VectorMaintenanceStepRequest,
) -> Result<VectorMaintenanceStepResult, String> {
    canister::admin_vector_maintenance_step(index_id, req)
}

/// Reports the vector-canister-owned maintenance execution state (ADR 0031 Slice 10). Router-guarded
/// `#[query]`; an index with no recorded state reports `Idle`.
#[query(guard = "guard_router_canister")]
fn admin_vector_maintenance_status(index_id: u32) -> Result<VectorMaintenanceState, String> {
    canister::admin_vector_maintenance_status(index_id)
}

/// Resets the maintenance execution state to `Idle` from any state, including `Failed` (ADR 0031
/// Slice 10). The only recovery path for a `Failed` maintenance state; does not touch the rebuild
/// state (abort an in-flight rebuild with `admin_abort_vector_rebuild`). Router-guarded `#[update]`.
#[update(guard = "guard_router_canister")]
fn admin_vector_maintenance_reset(index_id: u32) -> Result<(), String> {
    canister::admin_vector_maintenance_reset(index_id)
}

ic_cdk::export_candid!();
