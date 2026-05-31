#![cfg_attr(test, feature(f128))]

#[cfg(feature = "canbench")]
mod bench;
#[expect(
    dead_code,
    reason = "facade exposes canister storage helpers used by feature and integration paths"
)]
pub mod facade;
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

use ic_cdk_macros::{init, query, update};

use crate::canister::{
    GraphInitArgs,
    guards::{guard_control_plane_admin, guard_router_canister, guard_router_or_peer_graph},
};

#[init]
async fn init(args: GraphInitArgs) {
    canister::handlers::init(args).await;
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
fn bootstrap_graph_peers(
    args: gleaph_graph_kernel::federation::BootstrapGraphPeersArgs,
) -> Result<(), String> {
    canister::handlers::bootstrap_graph_peers(args)
}

#[update(guard = "guard_router_canister")]
fn add_graph_peer(args: gleaph_graph_kernel::federation::AddGraphPeerArgs) -> Result<(), String> {
    canister::handlers::add_graph_peer(args)
}

#[update(guard = "guard_router_canister")]
fn remove_graph_peer(
    args: gleaph_graph_kernel::federation::RemoveGraphPeerArgs,
) -> Result<(), String> {
    canister::handlers::remove_graph_peer(args)
}

#[update(guard = "guard_control_plane_admin")]
async fn migration_start(
    args: gleaph_graph_kernel::federation::BeginVertexMigrationArgs,
) -> Result<gleaph_graph_kernel::federation::MigrationStartResult, String> {
    canister::handlers::migration_start(args).await
}

#[update(guard = "guard_control_plane_admin")]
async fn migration_staging_begin(
    args: gleaph_graph_kernel::federation::MigrationStagingArgs,
) -> Result<gleaph_graph_kernel::federation::MigrationStartResult, String> {
    canister::handlers::migration_staging_begin(args).await
}

#[update(guard = "guard_control_plane_admin")]
async fn migration_apply_chunk(
    chunk: gleaph_graph_kernel::federation::MigrationApplyChunk,
) -> Result<(), String> {
    canister::handlers::migration_apply_chunk(chunk).await
}

#[update(guard = "guard_control_plane_admin")]
async fn migration_cutover(
    logical_vertex_id: gleaph_graph_kernel::federation::LogicalVertexId,
) -> Result<(), String> {
    canister::handlers::migration_cutover(logical_vertex_id).await
}

#[query(guard = "guard_control_plane_admin")]
fn migration_status(
    logical_vertex_id: gleaph_graph_kernel::federation::LogicalVertexId,
) -> Result<gleaph_graph_kernel::federation::MigrationStatus, String> {
    canister::handlers::migration_status_query(logical_vertex_id)
}

#[update(guard = "guard_control_plane_admin")]
async fn migration_maintenance_tick()
-> Result<Option<gleaph_graph_kernel::federation::MigrationApplyChunk>, String> {
    canister::handlers::migration_maintenance_tick().await
}

#[cfg(feature = "pocket-ic-e2e")]
#[update]
fn e2e_attach_federation(args: canister::types::E2eAttachFederationArgs) -> Result<(), String> {
    canister::handlers::e2e_attach_federation(args)
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
async fn e2e_insert_vertex() -> Result<canister::types::E2eInsertVertexResult, String> {
    canister::handlers::e2e_insert_vertex().await
}

#[cfg(feature = "pocket-ic-e2e")]
#[update(guard = "guard_control_plane_admin")]
fn e2e_insert_directed_edge(
    args: canister::types::E2eInsertDirectedEdgeArgs,
) -> Result<(), String> {
    canister::handlers::e2e_insert_directed_edge(args)
}

#[update(guard = "guard_control_plane_admin")]
async fn migration_reconcile(
    logical_vertex_id: gleaph_graph_kernel::federation::LogicalVertexId,
) -> Result<gleaph_graph_kernel::federation::MigrationReconcileReport, String> {
    canister::handlers::migration_reconcile_query(logical_vertex_id).await
}

#[query(composite = true, guard = "guard_router_or_peer_graph")]
async fn federated_expand(
    args: gleaph_graph_kernel::federation::FederatedExpandArgs,
) -> Result<Vec<gleaph_graph_kernel::federation::FederatedExpandNeighbor>, String> {
    canister::handlers::federated_expand(args).await
}

ic_cdk::export_candid!();
