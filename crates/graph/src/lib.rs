#![cfg_attr(test, feature(f128))]

#[cfg(feature = "canbench")]
mod bench;
mod edge_inline_value_scalar_codec;
mod edge_inline_value_schema;
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

// --- Canister surface (ic-cdk macros stay here; logic lives in `canister::`) ---

use ic_cdk_macros::{init, post_upgrade, query, update};

use crate::canister::guards::guard_control_plane_admin;
use crate::canister::{GraphInitArgs, guards::guard_router_canister};

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

#[update(guard = "guard_router_canister")]
async fn execute_plan_update_batch(
    args: gleaph_graph_kernel::plan_exec::ExecutePlanBatchArgs,
) -> Result<gleaph_graph_kernel::plan_exec::ExecutePlanBatchResult, String> {
    canister::handlers::execute_plan_update_batch(args).await
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
fn admin_set_vector_index_canister(vector_index_canister: candid::Principal) -> Result<(), String> {
    canister::handlers::admin_set_vector_index_canister(vector_index_canister)
}

/// Router → graph (plan 0048): bounded canonical vertex-embedding ingestion.
#[update(guard = "guard_router_canister")]
async fn admin_ingest_vertex_embedding(
    args: gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionArgs,
) -> Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, String> {
    canister::handlers::admin_ingest_vertex_embedding(args).await
}

/// Router → graph (plan 0048 extension): bounded batch canonical vertex-embedding ingestion.
#[update(guard = "guard_router_canister")]
async fn admin_ingest_vertex_embedding_batch(
    args: Vec<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionArgs>,
) -> Result<
    Vec<Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, String>>,
    String,
> {
    canister::handlers::admin_ingest_vertex_embedding_batch(args).await
}

/// Router → graph: operator-only physical stable-memory inventory.
#[query(guard = "guard_control_plane_admin")]
fn admin_stable_memory_stats() -> gleaph_graph_kernel::stable_memory::StableMemoryStats {
    canister::handlers::admin_stable_memory_stats()
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
async fn e2e_insert_directed_edge_with_inline_value(
    args: canister::types::E2eInsertDirectedEdgeWithPayloadArgs,
) -> Result<(), String> {
    canister::handlers::e2e_insert_directed_edge_with_inline_value(args).await
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
fn e2e_repair_journal_len() -> u64 {
    canister::handlers::e2e_repair_journal_len()
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
fn e2e_arm_unique_ack_fault(code: u8) -> Result<(), String> {
    canister::handlers::e2e_arm_unique_ack_fault(code)
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
async fn backfill_vertex_embeddings(
    req: gleaph_graph_kernel::federation::VertexEmbeddingBackfillRequest,
) -> Result<gleaph_graph_kernel::federation::EmbeddingBackfillResult, String> {
    canister::handlers::backfill_vertex_embeddings(req).await
}

#[update(guard = "guard_router_canister")]
fn finalize_bulk_ingest(
    args: gleaph_graph_kernel::federation::BulkIngestFinalizeArgs,
) -> Result<gleaph_graph_kernel::federation::BulkIngestFinalizeResult, String> {
    canister::handlers::finalize_bulk_ingest(args)
}

ic_cdk::export_candid!();
