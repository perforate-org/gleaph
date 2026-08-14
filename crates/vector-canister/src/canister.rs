//! Canister request handlers for `gleaph-vector-canister`.

use crate::facade::{
    VectorCanisterStore, VectorSyncBatchOutcomeOperationError, advance_watermark, gc_subjects_step,
};
use crate::init::VectorCanisterInitArgs;
use crate::state::VectorCanisterError;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ShardDetachCursor, ShardDetachStepResult, ShardId};
use gleaph_graph_kernel::vector_index::{
    VectorCentroidCacheStatus, VectorEmbeddingSyncOp, VectorMaintenancePolicy,
    VectorMaintenanceRecommendation, VectorMaintenanceState, VectorMaintenanceStepRequest,
    VectorMaintenanceStepResult, VectorPartitionHealthStep, VectorPartitionHealthSummary,
    VectorPartitionPageHealth, VectorRebuildStatus, VectorSearchRequest, VectorSearchResult,
    VectorSlabStats, VectorSlabStatsStep, VectorSyncBatchOutcome, VectorSyncBatchProgress,
    VectorSyncBatchUnavailable, VectorSyncTerminalError,
};
use ic_cdk::api::msg_caller;

pub(crate) const VECTOR_BATCH_MAX_INSTRUCTIONS: u64 = 32_000_000_000;
pub(crate) const VECTOR_BATCH_RESERVE_INSTRUCTIONS: u64 = 100_000_000;

/// Upper bound on subject-map entries a single GC step examines (ADR 0064 §5). Bounds the per-message
/// GC work so a large subject map cannot force an O(N) scan in one message.
pub(crate) const GC_SUBJECTS_BUDGET: u32 = 20_000;

#[inline]
pub(crate) fn instruction_counter() -> u64 {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::instruction_counter()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        0
    }
}

pub(crate) fn init(args: VectorCanisterInitArgs) {
    if let Err(e) = VectorCanisterStore::new().init_from_args(&args) {
        ic_cdk::trap(e.to_string());
    }
}

/// Rebind the definition and subject-store heap owners. Each retains an unavailable exact-open
/// result for subsequent request handling instead of creating or resetting a nonempty region.
pub(crate) fn post_upgrade() {
    VectorCanisterStore::new().open_definition_store_after_upgrade();
    VectorCanisterStore::new().open_subject_store_after_upgrade();
}

/// Test-only PocketIC seam for simulating the post-upgrade pre-open window without touching stable
/// memory. The definition owner must be `Ready`; production lifecycle code remains exact-open.
#[cfg(feature = "pocket-ic-e2e")]
pub(crate) fn e2e_unbind_definition_store() -> Result<(), String> {
    crate::facade::stable::definition_store::unbind_for_test().map_err(str::to_owned)
}

pub(crate) fn admin_attach_shard_canister(
    graph_id: GraphId,
    shard_id: ShardId,
    shard_canister_principal: Principal,
) -> Result<(), String> {
    VectorCanisterStore::new()
        .admin_attach_shard_canister(msg_caller(), graph_id, shard_id, shard_canister_principal)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_detach_shard_canister(
    shard_id: ShardId,
    resume: Option<ShardDetachCursor>,
) -> Result<ShardDetachStepResult, String> {
    VectorCanisterStore::new()
        .admin_detach_shard_canister(msg_caller(), shard_id, resume)
        .map_err(|e| e.to_string())
}

pub(crate) fn vector_upsert(op: VectorEmbeddingSyncOp) -> Result<(), VectorCanisterError> {
    VectorCanisterStore::new().vector_upsert(msg_caller(), &op)
}

pub(crate) fn vector_remove(op: VectorEmbeddingSyncOp) -> Result<(), VectorCanisterError> {
    VectorCanisterStore::new().vector_remove(msg_caller(), &op)
}

pub(crate) fn vector_sync_batch(
    operations: Vec<VectorEmbeddingSyncOp>,
) -> Result<VectorSyncBatchProgress, VectorCanisterError> {
    let caller = msg_caller();
    let store = VectorCanisterStore::new();
    let baseline = instruction_counter();
    let mut applied = 0u32;
    // Process the batch in bounded chunks so the per-op instruction-budget check becomes a per-chunk
    // check with a valid resume point (`next_index` is a chunk boundary). A chunk is small enough
    // that its append+commit stays far below the 100M reserve.
    const CHUNK: usize = 32;
    while (applied as usize) < operations.len() {
        let exhausted = instruction_counter()
            .saturating_sub(baseline)
            .saturating_add(VECTOR_BATCH_RESERVE_INSTRUCTIONS)
            >= VECTOR_BATCH_MAX_INSTRUCTIONS;
        if exhausted {
            return Ok(VectorSyncBatchProgress {
                applied,
                next_index: Some(applied),
                instruction_budget_exhausted: true,
            });
        }
        let end = ((applied as usize) + CHUNK).min(operations.len());
        let chunk = &operations[applied as usize..end];
        let applied_in_chunk = store.vector_sync_batch_chunk(caller, chunk)?;
        applied = applied.saturating_add(applied_in_chunk);
        if applied_in_chunk == 0 {
            break; // safety: never spin on a no-progress chunk
        }
    }
    // ADR 0064 §5: advance the caller's watermark to the max stamp in the batch, then run a bounded
    // GC step to drop deleted subject-map entries below the cutoff.
    crate::facade::advance_watermark(caller, &operations);
    crate::facade::gc_subjects_step(GC_SUBJECTS_BUDGET);
    Ok(VectorSyncBatchProgress {
        applied,
        next_index: None,
        instruction_budget_exhausted: false,
    })
}

/// Failure that cannot be represented as a committed typed batch outcome.
///
/// The public wrapper projects `StoreUnavailable` to the outer Candid error only before any
/// operation has committed.  `Fatal` always traps at the IC boundary so a prior prefix is rolled
/// back rather than being hidden behind an outer error.
#[derive(Debug)]
pub(crate) enum VectorSyncBatchOutcomeDriverError {
    StoreUnavailable,
    SubjectStoreUnavailable,
    Fatal(VectorCanisterError),
}

/// Per-operation typed batch driver used by the additive endpoint and unit tests.
///
/// Unlike the legacy chunked handler, every operation first obtains definition admission before
/// row/page/subject mutations. This makes `TablePressure` an exact terminal index and preserves
/// an acknowledged prefix only for that explicit terminal condition.
pub(crate) fn vector_sync_batch_outcome_for_caller(
    caller: Principal,
    operations: &[VectorEmbeddingSyncOp],
) -> Result<VectorSyncBatchOutcome, VectorSyncBatchOutcomeDriverError> {
    let store = VectorCanisterStore::new();
    let baseline = instruction_counter();
    let mut applied = 0u32;

    for operation in operations {
        // Always attempt the first operation: Graph treats a zero-progress nonempty response as
        // malformed, while a terminal pressure at position zero remains explicit and valid.
        let exhausted = applied > 0
            && instruction_counter()
                .saturating_sub(baseline)
                .saturating_add(VECTOR_BATCH_RESERVE_INSTRUCTIONS)
                >= VECTOR_BATCH_MAX_INSTRUCTIONS;
        if exhausted {
            break;
        }

        match store.vector_sync_batch_outcome_apply_one(caller, operation) {
            Ok(()) => {
                applied = applied
                    .checked_add(1)
                    .expect("vector sync batch outcome exceeds u32");
            }
            Err(VectorSyncBatchOutcomeOperationError::TablePressure) => {
                if applied > 0 {
                    advance_watermark(caller, &operations[..applied as usize]);
                    gc_subjects_step(GC_SUBJECTS_BUDGET);
                }
                return Ok(VectorSyncBatchOutcome::Terminal {
                    applied,
                    failed_index: applied,
                    error: VectorSyncTerminalError::IndexDefinitionTablePressure,
                });
            }
            Err(VectorSyncBatchOutcomeOperationError::SubjectTablePressure) => {
                if applied > 0 {
                    advance_watermark(caller, &operations[..applied as usize]);
                    gc_subjects_step(GC_SUBJECTS_BUDGET);
                }
                return Ok(VectorSyncBatchOutcome::Terminal {
                    applied,
                    failed_index: applied,
                    error: VectorSyncTerminalError::SubjectTablePressure,
                });
            }
            Err(VectorSyncBatchOutcomeOperationError::StoreUnavailable) if applied == 0 => {
                return Err(VectorSyncBatchOutcomeDriverError::StoreUnavailable);
            }
            Err(VectorSyncBatchOutcomeOperationError::StoreUnavailable) => {
                return Err(VectorSyncBatchOutcomeDriverError::Fatal(
                    VectorCanisterError::StableGrowFailed,
                ));
            }
            Err(VectorSyncBatchOutcomeOperationError::SubjectStoreUnavailable) if applied == 0 => {
                return Err(VectorSyncBatchOutcomeDriverError::SubjectStoreUnavailable);
            }
            Err(VectorSyncBatchOutcomeOperationError::SubjectStoreUnavailable) => {
                return Err(VectorSyncBatchOutcomeDriverError::Fatal(
                    VectorCanisterError::StableGrowFailed,
                ));
            }
            Err(VectorSyncBatchOutcomeOperationError::Fatal(error)) => {
                return Err(VectorSyncBatchOutcomeDriverError::Fatal(error));
            }
        }
    }

    if applied > 0 {
        advance_watermark(caller, &operations[..applied as usize]);
        gc_subjects_step(GC_SUBJECTS_BUDGET);
    }
    Ok(VectorSyncBatchOutcome::Progress { applied })
}

pub(crate) fn vector_sync_batch_outcome(
    operations: Vec<VectorEmbeddingSyncOp>,
) -> Result<VectorSyncBatchOutcome, VectorSyncBatchUnavailable> {
    match vector_sync_batch_outcome_for_caller(msg_caller(), &operations) {
        Ok(outcome) => Ok(outcome),
        Err(VectorSyncBatchOutcomeDriverError::StoreUnavailable) => {
            Err(VectorSyncBatchUnavailable::IndexDefinitionStoreUnavailable)
        }
        Err(VectorSyncBatchOutcomeDriverError::SubjectStoreUnavailable) => {
            Err(VectorSyncBatchUnavailable::SubjectStoreUnavailable)
        }
        Err(VectorSyncBatchOutcomeDriverError::Fatal(error)) => ic_cdk::trap(error.to_string()),
    }
}

pub(crate) fn vector_search(
    req: VectorSearchRequest,
) -> Result<VectorSearchResult, VectorCanisterError> {
    VectorCanisterStore::new().vector_search(&req)
}

pub(crate) fn admin_start_vector_rebuild(
    index_id: u32,
    nlist: u32,
    sample_limit: u32,
) -> Result<(), String> {
    VectorCanisterStore::new()
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
    VectorCanisterStore::new()
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
    VectorCanisterStore::new()
        .admin_vector_rebuild_step(msg_caller(), index_id, max_subjects)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_rebuild_status(index_id: u32) -> Result<VectorRebuildStatus, String> {
    VectorCanisterStore::new()
        .admin_vector_rebuild_status(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_publish_vector_rebuild(index_id: u32) -> Result<(), String> {
    VectorCanisterStore::new()
        .admin_publish_vector_rebuild(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_abort_vector_rebuild(index_id: u32) -> Result<(), String> {
    VectorCanisterStore::new()
        .admin_abort_vector_rebuild(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_rebuild_cleanup_step(
    index_id: u32,
    max_work: u32,
) -> Result<VectorRebuildStatus, String> {
    VectorCanisterStore::new()
        .admin_vector_rebuild_cleanup_step(msg_caller(), index_id, max_work)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_partition_health(
    index_id: u32,
) -> Result<VectorPartitionHealthSummary, String> {
    VectorCanisterStore::new()
        .admin_vector_partition_health(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_partition_health_step(
    index_id: u32,
    cursor: Option<Vec<u8>>,
    max_pages: u32,
) -> Result<VectorPartitionHealthStep, String> {
    VectorCanisterStore::new()
        .admin_vector_partition_health_step(msg_caller(), index_id, cursor, max_pages)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_centroid_cache_warmup(
    index_id: u32,
) -> Result<VectorCentroidCacheStatus, String> {
    VectorCanisterStore::new()
        .admin_vector_centroid_cache_warmup(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_centroid_cache_clear() -> Result<VectorCentroidCacheStatus, String> {
    VectorCanisterStore::new()
        .admin_vector_centroid_cache_clear(msg_caller())
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_centroid_cache_status() -> Result<VectorCentroidCacheStatus, String> {
    VectorCanisterStore::new()
        .admin_vector_centroid_cache_status(msg_caller())
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_slab_stats(index_id: Option<u32>) -> Result<VectorSlabStats, String> {
    VectorCanisterStore::new()
        .admin_vector_slab_stats(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_slab_stats_step(
    cursor: Option<Vec<u8>>,
    max_pages: u32,
    index_id: Option<u32>,
) -> Result<VectorSlabStatsStep, String> {
    VectorCanisterStore::new()
        .admin_vector_slab_stats_step(msg_caller(), cursor, max_pages, index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_maintenance_step(
    index_id: u32,
    req: VectorMaintenanceStepRequest,
) -> Result<VectorMaintenanceStepResult, String> {
    VectorCanisterStore::new()
        .admin_vector_maintenance_step(msg_caller(), index_id, req)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_maintenance_status(
    index_id: u32,
) -> Result<VectorMaintenanceState, String> {
    VectorCanisterStore::new()
        .admin_vector_maintenance_status(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_maintenance_reset(index_id: u32) -> Result<(), String> {
    VectorCanisterStore::new()
        .admin_vector_maintenance_reset(msg_caller(), index_id)
        .map_err(|e| e.to_string())
}
