//! Federated derived vector canister (`gleaph-vector-canister`).
//!
//! Owns the derived `ivf_flat` search structures and is the **sole owner of embedding bytes** (ADR
//! 0064 §1). Slice 2 is mutation-only: `vector_upsert` / `vector_remove` over a degenerate `ivf_flat`
//! page store (`nlist = 1`, `partition_id = 0`, no centroids, no search). Shard/canister attachments
//! are configured by the router via `admin_attach_shard_canister`.
//!
//! ## API visibility
//!
//! Admin APIs are router-only (`guard_router_canister`). Mutation updates authorize the caller
//! against the shard catalog inside the store and return [`VectorCanisterError`] over the wire.

mod code_tier;
mod facade;
mod records;

#[cfg(feature = "canbench")]
mod bench;

pub mod init;
pub mod state;

mod canister;
mod encoding;
mod guards;

pub use encoding::{EncodingError, EncodingRecord, ScoringKernel};
pub use init::VectorCanisterInitArgs;
pub use state::VectorCanisterError;

use crate::guards::guard_router_canister;
use candid::{Encode, Principal};
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
use ic_cdk_macros::{init, post_upgrade, query, update};

#[init]
fn init(args: VectorCanisterInitArgs) {
    canister::init(args);
}

#[post_upgrade]
fn post_upgrade() {
    canister::post_upgrade();
}

/// Test-only Router-guarded seam that leaves all stable bytes untouched and only drops the live
/// definition-store heap owner from `Ready` to `Uninitialized`.
#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_router_canister")]
fn e2e_unbind_definition_store() -> Result<(), String> {
    canister::e2e_unbind_definition_store()
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

/// Applies a contiguous Router-owned frontier for one attached shard and runs one bounded subject
/// tombstone-GC step. The store repeats the Router and exact-attachment checks below this guard.
#[update(guard = "guard_router_canister")]
fn admin_advance_router_frontier(shard_id: ShardId, frontier: u64) -> Result<(), String> {
    canister::admin_advance_router_frontier(shard_id, frontier)
}

/// Test-only Router-guarded exact frontier/GC probe. It reads through the storage owner and does
/// not expose a production status surface.
#[cfg(feature = "pocket-ic-e2e")]
#[query(guard = "guard_router_canister")]
fn e2e_frontier_probe(
    index_id: u32,
    shard_id: ShardId,
    vertex_id: u32,
) -> Result<canister::VectorIngressFrontierProbe, String> {
    canister::e2e_frontier_probe(index_id, shard_id, vertex_id)
}

/// Test-only Router-guarded receipt of successful frontier store calls. The receipt is heap-only
/// and therefore resets naturally when the Vector canister is upgraded.
#[cfg(feature = "pocket-ic-e2e")]
#[query(guard = "guard_router_canister")]
fn e2e_frontier_receipt_probe() -> (u64, Option<(u32, u64)>) {
    canister::e2e_frontier_receipt_probe()
}

#[update]
fn vector_upsert(op: VectorEmbeddingSyncOp) -> Result<(), VectorCanisterError> {
    canister::vector_upsert(op)
}

#[update]
fn vector_remove(op: VectorEmbeddingSyncOp) -> Result<(), VectorCanisterError> {
    canister::vector_remove(op)
}

#[update]
fn vector_sync_batch_outcome(
    operations: Vec<VectorEmbeddingSyncOp>,
) -> Result<VectorSyncBatchOutcome, VectorSyncBatchUnavailable> {
    let request_bytes =
        Encode!(&(&operations,)).expect("vector_sync_batch_outcome request encoding");
    if request_bytes.len() > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        ic_cdk::trap("vector_sync_batch_outcome request exceeds the safe payload limit");
    }
    canister::vector_sync_batch_outcome(operations)
}

/// Read-only exact `ivf_flat` top-k search (ADR 0031 Slice 5). Router-guarded like the
/// property-index reads so derived vectors cannot be queried directly, bypassing the Slice 4
/// activation/readiness gate; the Router is the activation-gated public surface.
#[query(guard = "guard_router_canister")]
fn vector_search(req: VectorSearchRequest) -> Result<VectorSearchResult, VectorCanisterError> {
    canister::vector_search(req)
}

/// Begins a production shadow-version rebuild for an `nlist > 1` index (ADR 0031 Slice 7). O(1);
/// the Router drives the subsequent bounded steps. `fine_nlist = Some(f)` (with `f >= 2`) starts
/// a two-level (`levels = 2`) rebuild whose target generation holds `nlist * f` packed leaves;
/// `None` keeps the unchanged flat lifecycle. `code_tier = Some(true)` builds the shadow
/// generation with the 1-bit RaBitQ first-stage code tier (Slice 6 / ADR 0078) — the public
/// encoding and advertised result quality are unchanged. `eps_query_bps` / `eps_fine_bps`
/// (Slice 9) freeze the target generation's per-level ε₂ pruning in basis points (`0` =
/// nearest-partition-only, `u32::MAX` = full scan); `None` = `0` (legacy pruning). Router-guarded
/// like the other admin endpoints.
#[update(guard = "guard_router_canister")]
fn admin_start_vector_rebuild(
    index_id: u32,
    nlist: u32,
    sample_limit: u32,
    fine_nlist: Option<u32>,
    code_tier: Option<bool>,
    eps_query_bps: Option<u32>,
    eps_fine_bps: Option<u32>,
) -> Result<(), String> {
    canister::admin_start_vector_rebuild(
        index_id,
        nlist,
        sample_limit,
        fine_nlist,
        code_tier,
        eps_query_bps,
        eps_fine_bps,
    )
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

/// Reports heap centroid cache status (entries/bytes/cap) (ADR 0031 Slice 9). The cache itself is
/// warmed automatically after post-upgrade store reopen and populated lazily by update-path
/// centroid reads; per-query hit/miss is not tracked (queries cannot commit counters on IC).
/// Router-guarded `#[query]`.
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

/// Begins an opt-in slab dead-space compaction (plan 0278): O(1) start that snapshots the source
/// range `[slab header end, occupied_tail)` and fails while a compaction is already active. The
/// Router then drives `admin_vector_slab_compact_step` to completion; pages appended after this
/// point land above the snapshot range and are never moved. Router-guarded `#[update]`.
#[update(guard = "guard_router_canister")]
fn admin_start_vector_slab_compact() -> Result<(), String> {
    canister::admin_start_vector_slab_compact()
}

/// Advances one bounded slab-compaction unit (plan 0278): one meta-map lap segment plus at most
/// one copy batch — each page's bytes persisted before its 16 B `VectorPageMeta.slab_offset` swap —
/// finalizing with a single `occupied_tail` rewind once a full lap finds nothing live inside the
/// snapshot range. `max_pages`/`max_bytes` are clamped server-side. Router-guarded `#[update]`.
#[update(guard = "guard_router_canister")]
fn admin_vector_slab_compact_step(
    max_pages: u32,
    max_bytes: u64,
) -> Result<VectorSlabCompactionStatus, String> {
    canister::admin_vector_slab_compact_step(max_pages, max_bytes)
}

/// Reports the O(1) scalar slab-compaction driver state (phase + cursors). Router-guarded
/// `#[query]`; an idle store reports phase `Idle` with zeroed cursors.
#[query(guard = "guard_router_canister")]
fn admin_vector_slab_compact_status() -> Result<VectorSlabCompactionStatus, String> {
    canister::admin_vector_slab_compact_status()
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
