//! Canister request handlers for `gleaph-graph-vector-index`.

use crate::facade::VectorIndexStore;
use crate::init::VectorIndexInitArgs;
use crate::state::VectorIndexError;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ShardDetachCursor, ShardDetachStepResult, ShardId};
use gleaph_graph_kernel::vector_index::{
    VectorCentroidCacheStatus, VectorEmbeddingSyncOp, VectorMaintenancePolicy,
    VectorMaintenanceRecommendation, VectorMaintenanceState, VectorMaintenanceStepRequest,
    VectorMaintenanceStepResult, VectorPartitionHealthStep, VectorPartitionHealthSummary,
    VectorPartitionPageHealth, VectorRebuildStatus, VectorSearchRequest, VectorSearchResult,
    VectorSlabStats, VectorSlabStatsStep,
};
use ic_cdk::api::msg_caller;

pub(crate) fn init(args: VectorIndexInitArgs) {
    if let Err(e) = VectorIndexStore::new().init_from_args(&args) {
        ic_cdk::trap(e.to_string());
    }
}

pub(crate) fn admin_attach_shard_canister(
    graph_id: GraphId,
    shard_id: ShardId,
    shard_canister_principal: Principal,
) -> Result<(), String> {
    VectorIndexStore::new()
        .admin_attach_shard_canister(msg_caller(), graph_id, shard_id, shard_canister_principal)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_detach_shard_canister(
    shard_id: ShardId,
    resume: Option<ShardDetachCursor>,
) -> Result<ShardDetachStepResult, String> {
    VectorIndexStore::new()
        .admin_detach_shard_canister(msg_caller(), shard_id, resume)
        .map_err(|e| e.to_string())
}

pub(crate) fn vector_upsert(op: VectorEmbeddingSyncOp) -> Result<(), VectorIndexError> {
    VectorIndexStore::new().vector_upsert(msg_caller(), &op)
}

pub(crate) fn vector_remove(op: VectorEmbeddingSyncOp) -> Result<(), VectorIndexError> {
    VectorIndexStore::new().vector_remove(msg_caller(), &op)
}

pub(crate) fn vector_search(
    req: VectorSearchRequest,
) -> Result<VectorSearchResult, VectorIndexError> {
    VectorIndexStore::new().vector_search(&req)
}

pub(crate) fn admin_start_vector_rebuild(
    index_id: u32,
    nlist: u32,
    sample_limit: u32,
) -> Result<(), String> {
    VectorIndexStore::new()
        .admin_start_vector_rebuild(msg_caller(), index_id, nlist, sample_limit)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_start_vector_rebuild_if_recommended(
    index_id: u32,
    attested_page_health: VectorPartitionPageHealth,
    policy: VectorMaintenancePolicy,
    target_nlist: Option<u32>,
    sample_limit: u32,
) -> Result<VectorMaintenanceRecommendation, String> {
    VectorIndexStore::new()
        .admin_start_vector_rebuild_if_recommended(
            msg_caller(),
            index_id,
            attested_page_health,
            policy,
            target_nlist,
            sample_limit,
        )
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_rebuild_step(
    index_id: u32,
    max_subjects: u32,
) -> Result<VectorRebuildStatus, String> {
    VectorIndexStore::new()
        .admin_vector_rebuild_step(msg_caller(), index_id, max_subjects)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_rebuild_status(index_id: u32) -> Result<VectorRebuildStatus, String> {
    VectorIndexStore::new()
        .admin_vector_rebuild_status(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_publish_vector_rebuild(index_id: u32) -> Result<(), String> {
    VectorIndexStore::new()
        .admin_publish_vector_rebuild(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_abort_vector_rebuild(index_id: u32) -> Result<(), String> {
    VectorIndexStore::new()
        .admin_abort_vector_rebuild(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_rebuild_cleanup_step(
    index_id: u32,
    max_work: u32,
) -> Result<VectorRebuildStatus, String> {
    VectorIndexStore::new()
        .admin_vector_rebuild_cleanup_step(msg_caller(), index_id, max_work)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_partition_health(
    index_id: u32,
) -> Result<VectorPartitionHealthSummary, String> {
    VectorIndexStore::new()
        .admin_vector_partition_health(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_partition_health_step(
    index_id: u32,
    cursor: Option<Vec<u8>>,
    max_pages: u32,
) -> Result<VectorPartitionHealthStep, String> {
    VectorIndexStore::new()
        .admin_vector_partition_health_step(msg_caller(), index_id, cursor, max_pages)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_centroid_cache_warmup(
    index_id: u32,
) -> Result<VectorCentroidCacheStatus, String> {
    VectorIndexStore::new()
        .admin_vector_centroid_cache_warmup(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_centroid_cache_clear() -> Result<VectorCentroidCacheStatus, String> {
    VectorIndexStore::new()
        .admin_vector_centroid_cache_clear(msg_caller())
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_centroid_cache_status() -> Result<VectorCentroidCacheStatus, String> {
    VectorIndexStore::new()
        .admin_vector_centroid_cache_status(msg_caller())
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_slab_stats(index_id: Option<u32>) -> Result<VectorSlabStats, String> {
    VectorIndexStore::new()
        .admin_vector_slab_stats(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_slab_stats_step(
    cursor: Option<Vec<u8>>,
    max_pages: u32,
    index_id: Option<u32>,
) -> Result<VectorSlabStatsStep, String> {
    VectorIndexStore::new()
        .admin_vector_slab_stats_step(msg_caller(), cursor, max_pages, index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_maintenance_step(
    index_id: u32,
    req: VectorMaintenanceStepRequest,
) -> Result<VectorMaintenanceStepResult, String> {
    VectorIndexStore::new()
        .admin_vector_maintenance_step(msg_caller(), index_id, req)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_maintenance_status(
    index_id: u32,
) -> Result<VectorMaintenanceState, String> {
    VectorIndexStore::new()
        .admin_vector_maintenance_status(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_maintenance_reset(index_id: u32) -> Result<(), String> {
    VectorIndexStore::new()
        .admin_vector_maintenance_reset(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}
