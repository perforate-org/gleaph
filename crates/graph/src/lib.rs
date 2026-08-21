#![cfg_attr(test, feature(f128))]
// wasm64 exposes the wasm SIMD intrinsics behind this nightly feature gate (rust-lang #90599);
// SIMD is enabled for canister builds via the workspace-root `.cargo/config.toml` target rustflags.
// `unused_features` fires on host builds where this gate is cfg'd out (clippy does not honor the
// cfg for this lint); the feature is genuinely required on wasm64.
#![cfg_attr(
    all(target_family = "wasm", target_arch = "wasm64"),
    feature(simd_wasm64)
)]
#![allow(unused_features)]

#[cfg(feature = "canbench")]
mod bench;
mod edge_inline_property_scalar_codec;
mod edge_inline_property_schema;
mod element_id_encoding;
#[expect(
    dead_code,
    reason = "facade exposes canister storage helpers used by feature and integration paths"
)]
pub mod facade;
mod federation;
pub mod gql_execution_context;
#[expect(
    dead_code,
    reason = "ad-hoc GQL helpers are retained for canister/debug entry points"
)]
pub mod gql_run;
#[expect(
    dead_code,
    reason = "index clients include IC/router implementations selected by deployment wiring"
)]
mod index;
/// Graph-owned canonical property-index export API (ADR 0059).
pub mod canonical_export {
    pub use crate::index::canonical_export::{
        abort_scope, activate_scope, export_page, register_scope, remove_scope, scope_status,
        seal_scope,
    };
    pub use gleaph_graph_kernel::canonical_export::CanonicalExportError;
}
mod plan_wire_guard;
mod property;
#[cfg(feature = "pocket-ic-e2e")]
mod test_fault;
#[cfg(any(test, feature = "canbench"))]
mod test_labels;

#[expect(
    dead_code,
    reason = "planner/executor contains optional operator and kernel paths"
)]
pub mod plan;

mod canister;
#[cfg(feature = "batch-instr-log")]
pub(crate) use canister::instr_log;

// --- Canister surface (ic-cdk macros stay here; logic lives in `canister::`) ---

use ic_cdk_macros::{init, post_upgrade, query, update};

use crate::canister::guards::guard_control_plane_admin;
use crate::canister::{
    GraphInitArgs,
    guards::{guard_index_canister, guard_router_canister},
};

#[init]
async fn init(args: GraphInitArgs) {
    canister::handlers::init(args).await;
}

/// Rebuilds non-stable process state after an upgrade: re-installs the rkyv
/// extension decode hook and re-arms the deferred-maintenance timer if the
/// (stable) queue is non-empty (timers do not survive upgrades; ADR 0020).
#[post_upgrade]
fn post_upgrade() {
    canister::handlers::post_upgrade();
}

/// Router → graph: read-only plan wire (may call index / federated expand).
#[query(composite = true, guard = "guard_router_canister")]
async fn execute_plan_query(
    args: gleaph_graph_kernel::plan_exec::ExecutePlanArgs,
) -> Result<gleaph_graph_kernel::plan_exec::ExecutePlanResult, String> {
    canister::handlers::execute_plan_query(args).await
}

/// Router → graph: plan wire with DML.
#[update(guard = "guard_router_canister")]
async fn execute_plan_update(
    args: gleaph_graph_kernel::plan_exec::ExecutePlanArgs,
) -> Result<gleaph_graph_kernel::plan_exec::ExecutePlanResult, String> {
    canister::handlers::execute_plan_update(args).await
}

/// Router → graph: journal-first ordered edge batch execution (ADR 0049).
#[update(guard = "guard_router_canister")]
async fn execute_ordered_edge_batch(
    args: gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphArgs,
) -> Result<gleaph_graph_kernel::plan_exec::GraphOrderedEdgeBatchResult, String> {
    canister::handlers::execute_ordered_edge_batch(args).await
}

/// Router → graph: journal-first ordered vertex bulk placement (ADR 0049).
#[update(guard = "guard_router_canister")]
async fn execute_ordered_vertex_batch(
    args: gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphArgs,
) -> Result<gleaph_graph_kernel::plan_exec::GraphOrderedVertexBatchResult, String> {
    canister::handlers::execute_ordered_vertex_batch(args).await
}

/// Router → graph: journal-first mixed vertex/edge batch execution (ADR 0049).
#[update(guard = "guard_router_canister")]
async fn execute_ordered_mixed_batch(
    args: gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphArgs,
) -> Result<gleaph_graph_kernel::plan_exec::GraphOrderedMixedBatchResult, String> {
    canister::handlers::execute_ordered_mixed_batch(args).await
}

#[update(guard = "guard_router_canister")]
async fn execute_plan_update_batch(
    args: gleaph_graph_kernel::plan_exec::ExecutePlanBatchArgs,
) -> Result<gleaph_graph_kernel::plan_exec::ExecutePlanBatchResult, String> {
    canister::handlers::execute_plan_update_batch(args).await
}

/// Router → graph: retire an exact ordered vertex mutation after projection convergence.
#[update(guard = "guard_router_canister")]
fn retire_ordered_vertex_mutation(
    args: gleaph_graph_kernel::plan_exec::OrderedVertexMutationRetirementArgs,
) -> Result<gleaph_graph_kernel::plan_exec::OrderedVertexMutationRetirementAck, String> {
    canister::handlers::retire_ordered_vertex_mutation(args)
}

#[query(guard = "guard_router_canister")]
fn list_pending_label_stats_deltas(
    from_seq: gleaph_graph_kernel::plan_exec::ShardEventSeq,
    limit: u32,
) -> Vec<gleaph_graph_kernel::plan_exec::LabelStatsDeltaEventWire> {
    canister::handlers::list_pending_label_stats_deltas(from_seq, limit)
}

#[query(guard = "guard_router_canister")]
fn get_mutation_journal_entry(
    mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
) -> Option<gleaph_graph_kernel::plan_exec::GraphMutationJournalEntryWire> {
    canister::handlers::get_mutation_journal_entry(mutation_id)
}

/// Router → graph: fingerprint-bound ordered mutation retirement (ADR 0049).
#[update(guard = "guard_router_canister")]
fn retire_ordered_mutation(
    args: gleaph_graph_kernel::plan_exec::OrderedMutationRetirementArgs,
) -> Result<gleaph_graph_kernel::plan_exec::OrderedMutationRetirementAck, String> {
    canister::handlers::retire_ordered_mutation(args)
}

/// Router → graph: fingerprint-bound ordered mixed mutation retirement (ADR 0049).
#[update(guard = "guard_router_canister")]
fn retire_ordered_mixed_mutation(
    args: gleaph_graph_kernel::plan_exec::OrderedMutationRetirementArgs,
) -> Result<gleaph_graph_kernel::plan_exec::OrderedMixedMutationRetirementAck, String> {
    canister::handlers::retire_ordered_mixed_mutation(args)
}

#[query(guard = "guard_router_canister")]
fn get_mutation_journal_entries(
    args: gleaph_graph_kernel::plan_exec::GetMutationJournalEntriesArgs,
) -> gleaph_graph_kernel::plan_exec::GetMutationJournalEntriesResult {
    canister::handlers::get_mutation_journal_entries(args)
}

/// Router → graph: smallest tracked mutation id with unapplied index postings, or
/// `None` when index work has drained (ADR 0029 Phase 2 read-your-writes barrier).
#[query(guard = "guard_router_canister")]
fn index_pending_min_mutation_id() -> Option<gleaph_graph_kernel::plan_exec::MutationId> {
    canister::handlers::index_pending_min_mutation_id()
}

/// Router → graph: index convergence snapshot (durable first-delivery outbox +
/// failed-flush repair journal). Seeding orchestration polls `converged` before
/// dispatching index-dependent edge waves.
#[query(guard = "guard_router_canister")]
fn index_sync_status() -> gleaph_graph_kernel::federation::IndexSyncStatus {
    canister::handlers::index_sync_status()
}

#[update(guard = "guard_router_canister")]
fn ack_label_stats_deltas_through(through_seq: gleaph_graph_kernel::plan_exec::ShardEventSeq) {
    canister::handlers::ack_label_stats_deltas_through(through_seq);
}

/// Router → graph (replicated read): per-claim `Acquire` commit proof from the pinned unique-effect
/// outbox (ADR 0030). An `update` so the answer is replicated/certified.
#[update(guard = "guard_router_canister")]
fn read_unique_effect_proof(
    claim_ids: Vec<gleaph_graph_kernel::federation::ClaimId>,
) -> Vec<gleaph_graph_kernel::federation::UniqueAcquireProof> {
    canister::handlers::read_unique_effect_proof(claim_ids)
}

/// Router → graph (replicated read): one page of a mutation's pinned `Release` effects, reconciled
/// by the Router per `owner_element_id` and paged by `effect_ordinal` cursor (ADR 0030 slice 5b). An
/// `update` so the answer is replicated/certified.
#[update(guard = "guard_router_canister")]
fn read_unique_release_effects(
    mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    after_ordinal: Option<u32>,
    limit: u32,
) -> Vec<gleaph_graph_kernel::federation::UniqueEffectReceipt> {
    canister::handlers::read_unique_release_effects(mutation_id, after_ordinal, limit)
}

/// Router → graph (replicated read): one page of all of a mutation's pinned effects (`Acquire` and
/// `Release`), paged by `effect_ordinal` cursor, for the Router's unified slice-6 effect recovery
/// (Driver 2). An `update` so the answer is replicated/certified.
#[update(guard = "guard_router_canister")]
fn read_unique_mutation_effects(
    mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    after_ordinal: Option<u32>,
    limit: u32,
) -> Vec<gleaph_graph_kernel::federation::UniqueEffectReceipt> {
    canister::handlers::read_unique_mutation_effects(mutation_id, after_ordinal, limit)
}

/// Router → graph: per-effect ack (unpin) of unique effects after the Router has durably applied
/// them (ADR 0030).
#[update(guard = "guard_router_canister")]
fn ack_unique_effects(effect_ids: Vec<gleaph_graph_kernel::federation::EffectId>) {
    canister::handlers::ack_unique_effects(effect_ids);
}

/// Router → graph: bounded purge of a `ShardLocalGlobal` constraint's local unique entries for the
/// DROP drain, returning whether the constraint's local table is now empty (ADR 0030 slice 10). An
/// update so the purge is replicated.
#[update(guard = "guard_router_canister")]
fn purge_local_unique_constraint(
    constraint_id: gleaph_graph_kernel::entry::ConstraintNameId,
    budget: u32,
) -> bool {
    canister::handlers::purge_local_unique_constraint(constraint_id, budget)
}

/// Router → graph (ADR 0031 Slice 4): set this shard's local derived vector-index target as the
/// first step of the Router-driven vector attach handshake.
#[update(guard = "guard_router_canister")]
fn admin_set_vector_canister(vector_canister: candid::Principal) -> Result<(), String> {
    canister::handlers::admin_set_vector_canister(vector_canister)
}

/// Router → graph: freeze one immutable canonical export scope for a physical index namespace.
#[update(guard = "guard_router_canister")]
fn admin_register_index_export_scope(
    physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId,
    scope: gleaph_graph_kernel::canonical_export::CanonicalExportScope,
) -> Result<(), gleaph_graph_kernel::canonical_export::CanonicalExportError> {
    canister::handlers::admin_register_index_export_scope(physical_index_id, scope)
}

/// Router → graph: remove an export scope under its complete current owner contract.
#[update(guard = "guard_router_canister")]
fn admin_remove_index_export_scope(
    physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId,
    scope: gleaph_graph_kernel::canonical_export::CanonicalExportScope,
) -> Result<(), gleaph_graph_kernel::canonical_export::CanonicalExportError> {
    canister::handlers::admin_remove_index_export_scope(physical_index_id, scope)
}

/// Router → graph: seal one export scope at a fresh catalog epoch.
#[update(guard = "guard_router_canister")]
fn admin_seal_index_export_scope(
    physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId,
    expected_scope: gleaph_graph_kernel::canonical_export::CanonicalExportScope,
    new_epoch: u64,
) -> Result<
    gleaph_graph_kernel::canonical_export::CanonicalExportStatus,
    gleaph_graph_kernel::canonical_export::CanonicalExportError,
> {
    canister::handlers::admin_seal_index_export_scope(physical_index_id, expected_scope, new_epoch)
}

/// Router → graph: publish one export scope `Active` after graph-index convergence.
#[update(guard = "guard_router_canister")]
fn admin_activate_index_export_scope(
    physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId,
    proof: gleaph_graph_kernel::index::IndexBuildSealStatus,
) -> Result<
    gleaph_graph_kernel::canonical_export::CanonicalExportStatus,
    gleaph_graph_kernel::canonical_export::CanonicalExportError,
> {
    canister::handlers::admin_activate_index_export_scope(physical_index_id, proof)
}

/// Router → graph: mark one export scope `Aborting` under its original registration identity.
#[update(guard = "guard_router_canister")]
fn admin_abort_index_export_scope(
    physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId,
    expected_scope: gleaph_graph_kernel::canonical_export::CanonicalExportScope,
) -> Result<
    gleaph_graph_kernel::canonical_export::CanonicalExportStatus,
    gleaph_graph_kernel::canonical_export::CanonicalExportError,
> {
    canister::handlers::admin_abort_index_export_scope(physical_index_id, expected_scope)
}

/// Router → graph: exact durable status for one export scope.
#[query(guard = "guard_router_canister")]
fn admin_index_export_scope_status(
    physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId,
) -> Result<
    gleaph_graph_kernel::canonical_export::CanonicalExportStatus,
    gleaph_graph_kernel::canonical_export::CanonicalExportError,
> {
    canister::handlers::admin_index_export_scope_status(physical_index_id)
}

/// Router → graph: bounded drain of one namespace's build-DML outbox entries to graph-index.
#[update(guard = "guard_router_canister")]
async fn admin_drain_index_build_outbox(
    request: gleaph_graph_kernel::canonical_export::IndexBuildOutboxDrainRequest,
) -> Result<
    gleaph_graph_kernel::canonical_export::IndexBuildOutboxDrainProgress,
    gleaph_graph_kernel::canonical_export::CanonicalExportError,
> {
    canister::handlers::admin_drain_index_build_outbox(request).await
}

/// Graph-index → graph: read one bounded page from canonical Graph storage.
#[update(guard = "guard_index_canister")]
fn index_export_page(
    request: gleaph_graph_kernel::canonical_export::CanonicalExportRequest,
) -> Result<
    gleaph_graph_kernel::canonical_export::CanonicalExportPage,
    gleaph_graph_kernel::canonical_export::CanonicalExportError,
> {
    canister::handlers::index_export_page(request)
}

/// Router → graph (ADR 0064 §6): validate an embedding ingestion and consume a `mutation_id` without
/// a DML mutation or canonical write. The Router then sends the bytes + stamp to the vector canister.
#[update(guard = "guard_router_canister")]
async fn stamp_embedding(
    args: gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionArgs,
) -> Result<u64, String> {
    canister::handlers::stamp_embedding(args).await
}

/// Router → graph: operator-only physical stable-memory inventory.
#[query(guard = "guard_control_plane_admin")]
fn admin_stable_memory_stats() -> gleaph_graph_kernel::stable_memory::StableMemoryStats {
    canister::handlers::admin_stable_memory_stats()
}

#[cfg(feature = "batch-instr-log")]
#[query(guard = "guard_control_plane_admin")]
fn admin_take_batch_instr_log(offset: u32, limit: u32) -> Vec<String> {
    crate::instr_log::dump()
        .into_iter()
        .skip(offset as usize)
        .take(limit.clamp(1, 10_000) as usize)
        .collect()
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_insert_vertex() -> Result<canister::types::E2eInsertVertexResult, String> {
    canister::handlers::e2e_insert_vertex().await
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_insert_vertex_with_property(
    args: canister::types::E2eInsertVertexWithPropertyArgs,
) -> Result<canister::types::E2eInsertVertexResult, String> {
    canister::handlers::e2e_insert_vertex_with_property(args).await
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_insert_vertex_with_two_properties(
    args: canister::types::E2eInsertVertexWithTwoPropertiesArgs,
) -> Result<canister::types::E2eInsertVertexResult, String> {
    canister::handlers::e2e_insert_vertex_with_two_properties(args).await
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_insert_vertex_with_label(
    args: canister::types::E2eInsertVertexWithLabelArgs,
) -> Result<canister::types::E2eInsertVertexResult, String> {
    canister::handlers::e2e_insert_vertex_with_label(args).await
}
#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_insert_vertex_with_label_and_property(
    args: canister::types::E2eInsertVertexWithLabelAndPropertyArgs,
) -> Result<canister::types::E2eInsertVertexResult, String> {
    canister::handlers::e2e_insert_vertex_with_label_and_property(args).await
}
#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_insert_vertex_with_label_and_two_properties(
    args: canister::types::E2eInsertVertexWithLabelAndTwoPropertiesArgs,
) -> Result<canister::types::E2eInsertVertexResult, String> {
    canister::handlers::e2e_insert_vertex_with_label_and_two_properties(args).await
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_set_vertex_property(
    args: canister::types::E2eSetVertexPropertyArgs,
) -> Result<(), String> {
    canister::handlers::e2e_set_vertex_property(args).await
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_insert_directed_edge_with_label(
    args: canister::types::E2eInsertDirectedEdgeWithLabelArgs,
) -> Result<(), String> {
    canister::handlers::e2e_insert_directed_edge_with_label(args).await
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_insert_directed_edge_with_inline_property(
    args: canister::types::E2eInsertDirectedEdgeWithInlinePropertyArgs,
) -> Result<(), String> {
    canister::handlers::e2e_insert_directed_edge_with_inline_property(args).await
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
fn e2e_insert_directed_edge(
    args: canister::types::E2eInsertDirectedEdgeArgs,
) -> Result<(), String> {
    canister::handlers::e2e_insert_directed_edge(args)
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_insert_directed_edge_with_property(
    args: canister::types::E2eInsertDirectedEdgeWithPropertyArgs,
) -> Result<(), String> {
    canister::handlers::e2e_insert_directed_edge_with_property(args).await
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_insert_undirected_edge_with_property(
    args: canister::types::E2eInsertUndirectedEdgeWithPropertyArgs,
) -> Result<(), String> {
    canister::handlers::e2e_insert_undirected_edge_with_property(args).await
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
fn e2e_enqueue_forward_compaction(
    args: canister::types::E2eEnqueueForwardCompactionArgs,
) -> Result<(), String> {
    canister::handlers::e2e_enqueue_forward_compaction(args)
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_delete_directed_edge_with_property(
    args: canister::types::E2eDeleteDirectedEdgeArgs,
) -> Result<(), String> {
    canister::handlers::e2e_delete_directed_edge_with_property(args).await
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_set_edge_property(
    args: canister::types::E2eSetEdgePropertyArgs,
) -> Result<(), String> {
    canister::handlers::e2e_set_edge_property(args).await
}

#[cfg(feature = "pocket-ic-e2e")]
#[query(guard = "guard_control_plane_admin")]
fn e2e_maintenance_queue_len() -> u64 {
    canister::handlers::e2e_maintenance_queue_len()
}

#[cfg(feature = "pocket-ic-e2e")]
#[query(guard = "guard_control_plane_admin")]
fn e2e_derived_index_outbox_len() -> u64 {
    canister::handlers::e2e_derived_index_outbox_len()
}

#[cfg(feature = "pocket-ic-e2e")]
#[query(guard = "guard_control_plane_admin")]
fn e2e_derived_index_outbox_pending_is_empty() -> bool {
    canister::handlers::e2e_derived_index_outbox_pending_is_empty()
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
fn e2e_enqueue_vector_outbox(
    operations: Vec<gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp>,
) -> Result<(), String> {
    canister::handlers::e2e_enqueue_vector_outbox(operations)
}

#[cfg(feature = "pocket-ic-e2e")]
#[query(guard = "guard_control_plane_admin")]
fn e2e_repair_journal_len() -> u64 {
    canister::handlers::e2e_repair_journal_len()
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
fn e2e_arm_unique_ack_fault(code: u8) -> Result<(), String> {
    canister::handlers::e2e_arm_unique_ack_fault(code)
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
fn e2e_arm_ordered_response_loss(code: u8) -> Result<(), String> {
    canister::handlers::e2e_arm_ordered_response_loss(code)
}

#[cfg(feature = "pocket-ic-e2e")]
#[query(guard = "guard_control_plane_admin")]
fn e2e_ordered_dispatch_state() -> Result<(u64, bool), String> {
    canister::handlers::e2e_ordered_dispatch_state()
}

#[cfg(feature = "pocket-ic-e2e")]
#[query(guard = "guard_control_plane_admin")]
fn e2e_unique_outbox_len() -> Result<u64, String> {
    canister::handlers::e2e_unique_outbox_len()
}

#[cfg(feature = "pocket-ic-e2e")]
#[query(guard = "guard_control_plane_admin")]
fn e2e_mutation_journal_len() -> Result<u64, String> {
    canister::handlers::e2e_mutation_journal_len()
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
fn e2e_evict_mutation_journal() -> Result<u64, String> {
    canister::handlers::e2e_evict_mutation_journal()
}

#[cfg(feature = "pocket-ic-e2e")]
#[query(guard = "guard_control_plane_admin")]
fn e2e_reverse_resolved_edge_property(
    args: canister::types::E2eReverseResolvedEdgePropertyArgs,
) -> Result<Option<i64>, String> {
    canister::handlers::e2e_reverse_resolved_edge_property(args)
}

#[update(guard = "guard_router_canister")]
async fn backfill_label_postings(
    args: gleaph_graph_kernel::federation::PostingBackfillArgs,
) -> Result<gleaph_graph_kernel::federation::PostingBackfillResult, String> {
    canister::handlers::backfill_label_postings(args).await
}

#[update(guard = "guard_router_canister")]
async fn backfill_vertex_property_postings(
    req: gleaph_graph_kernel::federation::VertexPropertyBackfillRequest,
) -> Result<gleaph_graph_kernel::federation::PostingBackfillResult, String> {
    canister::handlers::backfill_vertex_property_postings(req).await
}

#[update(guard = "guard_router_canister")]
async fn backfill_edge_property_postings(
    req: gleaph_graph_kernel::federation::EdgePropertyBackfillRequest,
) -> Result<gleaph_graph_kernel::federation::EdgePostingBackfillResult, String> {
    canister::handlers::backfill_edge_property_postings(req).await
}

#[update(guard = "guard_router_canister")]
fn finalize_bulk_ingest(
    args: gleaph_graph_kernel::federation::BulkIngestFinalizeArgs,
) -> Result<gleaph_graph_kernel::federation::BulkIngestFinalizeResult, String> {
    canister::handlers::finalize_bulk_ingest(args)
}

ic_cdk::export_candid!();
