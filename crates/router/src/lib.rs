//! Gleaph router canister — federation control plane (graph registry, shard registry, placement).

mod canister;
pub mod facade;
mod index_sync;
pub mod init;
pub mod state;
pub mod types;

pub use facade::store::RouterStore;
pub use init::RouterInitArgs;
pub use state::RouterError;

use candid::Principal;
use ic_cdk_macros::{init, query, update};

#[init]
fn init(args: RouterInitArgs) {
    canister::init(args);
}

#[query]
fn whoami() -> Principal {
    canister::whoami()
}

#[query]
fn resolve_graph(
    graph_name: String,
) -> Result<gleaph_gql_ic::graph_registry::GraphRegistryEntry, RouterError> {
    canister::resolve_graph(graph_name)
}

#[query]
fn resolve_shard(shard_id: types::ShardId) -> Result<types::ShardRegistryEntry, RouterError> {
    canister::resolve_shard(shard_id)
}

#[query]
fn resolve_placement(
    logical_vertex_id: types::LogicalVertexId,
) -> Result<types::VertexPlacement, RouterError> {
    canister::resolve_placement(logical_vertex_id)
}

#[query]
fn resolve_logical_at(
    shard_id: types::ShardId,
    local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
) -> Result<types::LogicalVertexId, RouterError> {
    canister::resolve_logical_at(shard_id, local_vertex_id)
}

#[query]
fn lookup_vertex_label_id(name: String) -> Result<types::VertexLabelId, RouterError> {
    canister::lookup_vertex_label_id(name)
}

#[query]
fn lookup_edge_label_id(name: String) -> Result<types::EdgeLabelId, RouterError> {
    canister::lookup_edge_label_id(name)
}

#[query]
fn lookup_property_id(name: String) -> Result<types::PropertyId, RouterError> {
    canister::lookup_property_id(name)
}

#[query]
fn reverse_vertex_label_name(label_id: types::VertexLabelId) -> Result<String, RouterError> {
    canister::reverse_vertex_label_name(label_id)
}

#[query]
fn reverse_edge_label_name(label_id: types::EdgeLabelId) -> Result<String, RouterError> {
    canister::reverse_edge_label_name(label_id)
}

#[query]
fn reverse_property_name(property_id: types::PropertyId) -> Result<String, RouterError> {
    canister::reverse_property_name(property_id)
}

#[update]
fn admin_register_graph(
    entry: gleaph_gql_ic::graph_registry::GraphRegistryEntry,
) -> Result<(), RouterError> {
    canister::admin_register_graph(entry)
}

#[update]
fn admin_update_graph_status(
    graph_name: String,
    status: gleaph_gql_ic::graph_registry::GraphStatus,
    version: u64,
) -> Result<(), RouterError> {
    canister::admin_update_graph_status(graph_name, status, version)
}

#[update]
async fn admin_register_shard(args: types::AdminRegisterShardArgs) -> Result<(), RouterError> {
    canister::admin_register_shard(args).await
}

#[update]
async fn admin_unregister_shard(shard_id: types::ShardId) -> Result<(), RouterError> {
    canister::admin_unregister_shard(shard_id).await
}

#[update]
fn admin_intern_vertex_label(name: String) -> Result<types::VertexLabelId, RouterError> {
    canister::admin_intern_vertex_label(name)
}

#[update]
fn admin_intern_edge_label(name: String) -> Result<types::EdgeLabelId, RouterError> {
    canister::admin_intern_edge_label(name)
}

#[update]
fn admin_intern_property(name: String) -> Result<types::PropertyId, RouterError> {
    canister::admin_intern_property(name)
}

#[update]
fn allocate_logical_vertex_id() -> Result<types::LogicalVertexId, RouterError> {
    canister::allocate_logical_vertex_id()
}

#[update]
fn commit_vertex_placement(
    args: types::CommitVertexPlacementArgs,
) -> Result<(), RouterError> {
    canister::commit_vertex_placement(args)
}

#[update]
fn begin_vertex_migration(args: types::BeginVertexMigrationArgs) -> Result<(), RouterError> {
    canister::begin_vertex_migration(args)
}

#[update]
fn finish_vertex_migration(args: types::FinishVertexMigrationArgs) -> Result<(), RouterError> {
    canister::finish_vertex_migration(args)
}

ic_cdk::export_candid!();
