#![cfg_attr(test, feature(f128))]

#[cfg(feature = "canbench")]
mod bench;
mod edge_payload_schema;
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
#[cfg(any(test, feature = "canbench"))]
mod test_labels;

#[expect(
    dead_code,
    reason = "planner/executor contains optional operator and kernel paths"
)]
pub mod plan;

#[expect(
    dead_code,
    reason = "canister helpers are reached through IC macros and deployment features"
)]
mod canister;

// --- Canister surface (ic-cdk macros stay here; logic lives in `canister::`) ---

use ic_cdk_macros::{init, post_upgrade, query, update};

#[cfg(feature = "pocket-ic-e2e")]
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

#[update(guard = "guard_router_canister")]
fn ack_label_stats_deltas_through(through_seq: gleaph_graph_kernel::plan_exec::ShardEventSeq) {
    canister::handlers::ack_label_stats_deltas_through(through_seq);
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
#[query(guard = "guard_control_plane_admin")]
fn e2e_maintenance_queue_len() -> u64 {
    canister::handlers::e2e_maintenance_queue_len()
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
