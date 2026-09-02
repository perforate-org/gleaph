//! Canister request handlers for `gleaph-vector-canister`.

use crate::facade::VectorSyncBatchOutcomeOperationError;
use crate::facade::store;
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
    VectorSlabCompactionStatus, VectorSlabStats, VectorSlabStatsStep, VectorSyncBatchOutcome,
    VectorSyncBatchUnavailable,
};
use ic_cdk::api::msg_caller;

#[cfg(feature = "pocket-ic-e2e")]
use std::cell::Cell;

pub(crate) const VECTOR_BATCH_MAX_INSTRUCTIONS: u64 = 32_000_000_000;
pub(crate) const VECTOR_BATCH_RESERVE_INSTRUCTIONS: u64 = 100_000_000;
const VECTOR_SYNC_BATCH_CHUNK: usize = 32;

/// Upper bound on subject-map entries a single GC step examines (ADR 0064 §5). Bounds the per-message
/// GC work so a large subject map cannot force an O(N) scan in one message.
pub(crate) const GC_SUBJECTS_BUDGET: u32 = 20_000;

#[cfg(feature = "pocket-ic-e2e")]
thread_local! {
    static E2E_FRONTIER_RECEIPT: Cell<(u64, Option<(u32, u64)>)> = const { Cell::new((0, None)) };
}

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

/// Advances an attached Graph's watermark and runs bounded subject GC for exactly the committed
/// prefix. Router direct-ingest calls have no contiguous watermark owner and skip both actions.
///
/// A batch may stop before the submitted suffix (for example at the instruction budget or a
/// typed terminal admission result). Advancing from the full request would make the suffix appear
/// durable and could collect deleted subject clocks before their operations are applied.
pub(crate) fn advance_watermark_and_gc_for_applied_prefix(
    caller: Principal,
    operations: &[VectorEmbeddingSyncOp],
    applied: u32,
) {
    let applied = usize::try_from(applied).expect("vector sync applied count exceeds usize");
    assert!(
        applied <= operations.len(),
        "vector sync applied count exceeds submitted operation count"
    );
    if applied == 0 {
        return;
    }
    if crate::facade::advance_watermark(caller, &operations[..applied]) {
        crate::facade::gc_subjects_step(GC_SUBJECTS_BUDGET);
    }
}

pub(crate) fn init(args: VectorCanisterInitArgs) {
    if let Err(e) = store::init_from_args(&args) {
        ic_cdk::trap(e.to_string());
    }
}

/// Rebind the definition and subject-store heap owners. Each retains an unavailable exact-open
/// result for subsequent request handling instead of creating or resetting a nonempty region.
/// Afterwards, restore the derived heap centroid cache so a reopened canister serves centroid
/// scoring warm without an operator round-trip (internal; no caller guard).
pub(crate) fn post_upgrade() {
    store::open_definition_store_after_upgrade();
    store::open_subject_store_after_upgrade();
    store::warm_all();
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
    store::admin_attach_shard_canister(msg_caller(), graph_id, shard_id, shard_canister_principal)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_detach_shard_canister(
    shard_id: ShardId,
    resume: Option<ShardDetachCursor>,
) -> Result<ShardDetachStepResult, String> {
    store::admin_detach_shard_canister(msg_caller(), shard_id, resume).map_err(|e| e.to_string())
}

pub(crate) fn admin_advance_router_frontier(
    shard_id: ShardId,
    frontier: u64,
) -> Result<(), String> {
    let result =
        store::advance_router_frontier(msg_caller(), shard_id, frontier).map_err(|e| e.to_string());
    #[cfg(feature = "pocket-ic-e2e")]
    if result.is_ok() {
        let receipt = (shard_id.raw(), frontier);
        E2E_FRONTIER_RECEIPT.with(|state| {
            let (count, _) = state.get();
            let next_count = count
                .checked_add(1)
                .expect("pocket-ic-e2e frontier receipt count overflow");
            state.set((next_count, Some(receipt)));
        });
    }
    result
}

#[cfg(feature = "pocket-ic-e2e")]
pub(crate) fn e2e_frontier_receipt_probe() -> (u64, Option<(u32, u64)>) {
    E2E_FRONTIER_RECEIPT.with(Cell::get)
}

#[cfg(feature = "pocket-ic-e2e")]
/// Ingress-facing alias of the PocketIC frontier probe tuple (see
/// [`store::watermark::VectorTombstoneFrontierProbe`]).
#[cfg(feature = "pocket-ic-e2e")]
pub(crate) type VectorIngressFrontierProbe = crate::facade::store::VectorTombstoneFrontierProbe;

#[cfg(feature = "pocket-ic-e2e")]
pub(crate) fn e2e_frontier_probe(
    index_id: u32,
    shard_id: ShardId,
    vertex_id: u32,
) -> Result<VectorIngressFrontierProbe, String> {
    store::test_vector_frontier_probe(index_id, shard_id, vertex_id)
}

pub(crate) fn vector_upsert(op: VectorEmbeddingSyncOp) -> Result<(), VectorCanisterError> {
    store::vector_upsert(msg_caller(), &op)
}

pub(crate) fn vector_remove(op: VectorEmbeddingSyncOp) -> Result<(), VectorCanisterError> {
    store::vector_remove(msg_caller(), &op)
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

/// Typed batch driver used by the additive endpoint and unit tests. Each bounded chunk uses the
/// page-store batch append for compatible upserts while preserving the exact outcome prefix across
/// chunk boundaries.
pub(crate) fn vector_sync_batch_outcome_for_caller(
    caller: Principal,
    operations: &[VectorEmbeddingSyncOp],
) -> Result<VectorSyncBatchOutcome, VectorSyncBatchOutcomeDriverError> {
    let baseline = instruction_counter();

    let prepared = match store::preflight_vector_sync_batch(caller, operations) {
        Ok(prepared) => prepared,
        Err(VectorSyncBatchOutcomeOperationError::StoreUnavailable) => {
            return Err(VectorSyncBatchOutcomeDriverError::StoreUnavailable);
        }
        Err(VectorSyncBatchOutcomeOperationError::SubjectStoreUnavailable) => {
            return Err(VectorSyncBatchOutcomeDriverError::SubjectStoreUnavailable);
        }
        Err(VectorSyncBatchOutcomeOperationError::Fatal(error)) => {
            return Err(VectorSyncBatchOutcomeDriverError::Fatal(error));
        }
        Err(VectorSyncBatchOutcomeOperationError::TablePressure)
        | Err(VectorSyncBatchOutcomeOperationError::SubjectTablePressure) => {
            unreachable!("typed batch preflight does not admit a terminal pressure result")
        }
    };

    let mut applied = 0u32;

    while (applied as usize) < operations.len() {
        // Always attempt the first chunk: Graph treats a zero-progress nonempty response as
        // malformed, while a terminal pressure at position zero remains explicit and valid.
        let exhausted = applied > 0
            && instruction_counter()
                .saturating_sub(baseline)
                .saturating_add(VECTOR_BATCH_RESERVE_INSTRUCTIONS)
                >= VECTOR_BATCH_MAX_INSTRUCTIONS;
        if exhausted {
            break;
        }

        let offset = applied as usize;
        let end = offset
            .checked_add(VECTOR_SYNC_BATCH_CHUNK)
            .unwrap_or(operations.len())
            .min(operations.len());
        let chunk = &operations[offset..end];
        let outcome =
            match store::vector_sync_batch_outcome_chunk(caller, chunk, &prepared[offset..end]) {
                Ok(outcome) => outcome,
                Err(VectorSyncBatchOutcomeOperationError::StoreUnavailable) if applied == 0 => {
                    return Err(VectorSyncBatchOutcomeDriverError::StoreUnavailable);
                }
                Err(VectorSyncBatchOutcomeOperationError::SubjectStoreUnavailable)
                    if applied == 0 =>
                {
                    return Err(VectorSyncBatchOutcomeDriverError::SubjectStoreUnavailable);
                }
                Err(VectorSyncBatchOutcomeOperationError::StoreUnavailable)
                | Err(VectorSyncBatchOutcomeOperationError::SubjectStoreUnavailable) => {
                    return Err(VectorSyncBatchOutcomeDriverError::Fatal(
                        VectorCanisterError::StableGrowFailed,
                    ));
                }
                Err(VectorSyncBatchOutcomeOperationError::Fatal(error)) => {
                    return Err(VectorSyncBatchOutcomeDriverError::Fatal(error));
                }
                Err(VectorSyncBatchOutcomeOperationError::TablePressure)
                | Err(VectorSyncBatchOutcomeOperationError::SubjectTablePressure) => {
                    unreachable!("typed batch chunk converts pressure to a terminal outcome")
                }
            };

        match outcome {
            VectorSyncBatchOutcome::Progress {
                applied: chunk_applied,
            } => {
                assert_eq!(
                    usize::try_from(chunk_applied).expect("chunk applied count exceeds usize"),
                    chunk.len(),
                    "typed batch chunk must make progress across its complete prefix"
                );
                applied = applied
                    .checked_add(chunk_applied)
                    .expect("vector sync batch outcome exceeds u32");
            }
            VectorSyncBatchOutcome::Terminal {
                applied: chunk_applied,
                failed_index,
                error,
            } => {
                let applied_total = applied
                    .checked_add(chunk_applied)
                    .expect("vector sync batch applied count exceeds u32");
                let failed_total = applied
                    .checked_add(failed_index)
                    .expect("vector sync batch failed index exceeds u32");
                advance_watermark_and_gc_for_applied_prefix(caller, operations, applied_total);
                return Ok(VectorSyncBatchOutcome::Terminal {
                    applied: applied_total,
                    failed_index: failed_total,
                    error,
                });
            }
        }
    }

    if applied > 0 {
        advance_watermark_and_gc_for_applied_prefix(caller, operations, applied);
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
    store::vector_search(&req)
}

pub(crate) fn admin_start_vector_rebuild(
    index_id: u32,
    nlist: u32,
    sample_limit: u32,
    fine_nlist: Option<u32>,
    code_tier: Option<bool>,
    eps_query_bps: Option<u32>,
    eps_fine_bps: Option<u32>,
) -> Result<(), String> {
    store::admin_start_vector_rebuild_with_fine(
        msg_caller(),
        index_id,
        nlist,
        sample_limit,
        fine_nlist,
        code_tier,
        eps_query_bps,
        eps_fine_bps,
    )
    .map_err(|e| e.to_string())
}

pub(crate) fn admin_start_vector_rebuild_if_recommended(
    index_id: u32,
    attested_page_health: VectorPartitionPageHealth,
    policy: VectorMaintenancePolicy,
    target_nlist: Option<u32>,
    sample_limit: u32,
) -> Result<VectorMaintenanceRecommendation, String> {
    store::admin_start_vector_rebuild_if_recommended(
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
    store::admin_vector_rebuild_step(msg_caller(), index_id, max_subjects)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_rebuild_status(index_id: u32) -> Result<VectorRebuildStatus, String> {
    store::admin_vector_rebuild_status(msg_caller(), index_id).map_err(|e| e.to_string())
}

pub(crate) fn admin_publish_vector_rebuild(index_id: u32) -> Result<(), String> {
    store::admin_publish_vector_rebuild(msg_caller(), index_id).map_err(|e| e.to_string())
}

pub(crate) fn admin_abort_vector_rebuild(index_id: u32) -> Result<(), String> {
    store::admin_abort_vector_rebuild(msg_caller(), index_id).map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_rebuild_cleanup_step(
    index_id: u32,
    max_work: u32,
) -> Result<VectorRebuildStatus, String> {
    store::admin_vector_rebuild_cleanup_step(msg_caller(), index_id, max_work)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_partition_health(
    index_id: u32,
) -> Result<VectorPartitionHealthSummary, String> {
    store::admin_vector_partition_health(msg_caller(), index_id).map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_partition_health_step(
    index_id: u32,
    cursor: Option<Vec<u8>>,
    max_pages: u32,
) -> Result<VectorPartitionHealthStep, String> {
    store::admin_vector_partition_health_step(msg_caller(), index_id, cursor, max_pages)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_centroid_cache_status() -> Result<VectorCentroidCacheStatus, String> {
    store::admin_vector_centroid_cache_status(msg_caller()).map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_slab_stats(index_id: Option<u32>) -> Result<VectorSlabStats, String> {
    store::admin_vector_slab_stats(msg_caller(), index_id).map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_slab_stats_step(
    cursor: Option<Vec<u8>>,
    max_pages: u32,
    index_id: Option<u32>,
) -> Result<VectorSlabStatsStep, String> {
    store::admin_vector_slab_stats_step(msg_caller(), cursor, max_pages, index_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_start_vector_slab_compact() -> Result<(), String> {
    store::admin_start_vector_slab_compact(msg_caller()).map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_slab_compact_step(
    max_pages: u32,
    max_bytes: u64,
) -> Result<VectorSlabCompactionStatus, String> {
    store::admin_vector_slab_compact_step(msg_caller(), max_pages, max_bytes)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_slab_compact_status() -> Result<VectorSlabCompactionStatus, String> {
    store::admin_vector_slab_compact_status(msg_caller()).map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_maintenance_step(
    index_id: u32,
    req: VectorMaintenanceStepRequest,
) -> Result<VectorMaintenanceStepResult, String> {
    store::admin_vector_maintenance_step(msg_caller(), index_id, req).map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_maintenance_status(
    index_id: u32,
) -> Result<VectorMaintenanceState, String> {
    store::admin_vector_maintenance_status(msg_caller(), index_id).map_err(|e| e.to_string())
}

pub(crate) fn admin_vector_maintenance_reset(index_id: u32) -> Result<(), String> {
    store::admin_vector_maintenance_reset(msg_caller(), index_id).map_err(|e| e.to_string())
}
