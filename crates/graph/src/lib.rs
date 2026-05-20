#![cfg_attr(test, feature(f128))]

#[cfg(feature = "canbench")]
mod bench;
pub mod facade;
pub mod gql_execution_context;
pub mod gql_run;
mod index;
mod plan_wire_guard;

pub mod plan;

mod canister;

// --- Canister surface (ic-cdk macros stay here; logic lives in `canister::`) ---

use ic_cdk_macros::{init, query, update};

use crate::canister::{
    GraphInitArgs,
    guards::{
        guard_control_plane_admin, guard_router_canister, guard_router_or_peer_graph,
    },
};

#[init]
fn init(args: GraphInitArgs) {
    canister::handlers::init(args);
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
fn add_graph_peer(
    args: gleaph_graph_kernel::federation::AddGraphPeerArgs,
) -> Result<(), String> {
    canister::handlers::add_graph_peer(args)
}

#[update(guard = "guard_router_canister")]
fn remove_graph_peer(
    args: gleaph_graph_kernel::federation::RemoveGraphPeerArgs,
) -> Result<(), String> {
    canister::handlers::remove_graph_peer(args)
}

#[update(guard = "guard_control_plane_admin")]
fn migration_begin(
    args: gleaph_graph_kernel::federation::BeginVertexMigrationArgs,
) -> Result<(), String> {
    canister::handlers::migration_begin(args)
}

#[query(guard = "guard_router_or_peer_graph")]
fn federated_expand(
    args: gleaph_graph_kernel::federation::FederatedExpandArgs,
) -> Result<Vec<gleaph_graph_kernel::federation::FederatedExpandNeighbor>, String> {
    canister::handlers::federated_expand(args)
}

#[query(guard = "guard_control_plane_admin")]
fn migration_export(
    local_vertex_id: u32,
) -> Result<gleaph_graph_kernel::federation::ExportedVertex, String> {
    canister::handlers::migration_export(local_vertex_id)
}

#[update(guard = "guard_control_plane_admin")]
async fn migration_import(
    bundle: gleaph_graph_kernel::federation::ExportedVertex,
) -> Result<u32, String> {
    canister::handlers::migration_import(bundle).await
}

#[update(guard = "guard_control_plane_admin")]
async fn migration_tombstone(local_vertex_id: u32) -> Result<(), String> {
    canister::handlers::migration_tombstone(local_vertex_id).await
}

ic_cdk::export_candid!();
