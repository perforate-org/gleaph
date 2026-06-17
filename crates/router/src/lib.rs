//! Gleaph router canister — federation control plane (graph registry, shard registry, placement).

#[cfg(feature = "canbench")]
mod bench;

mod bulk_ingest_finalize;
mod canister;
mod edge_backfill;
mod edge_index_direction;
mod execution_path;
pub mod facade;
mod federation;
mod gql;
mod graph_client;
mod graph_context;
mod index_catalog;
#[cfg_attr(
    not(target_family = "wasm"),
    expect(dead_code, reason = "index client issues IC calls only on wasm")
)]
mod index_client;
mod index_ddl;
mod index_lookup;
mod index_route;
mod index_sync;
pub mod init;
mod label_backfill;
mod label_stats_projection;
#[cfg_attr(
    not(target_family = "wasm"),
    expect(
        dead_code,
        reason = "peer sync hooks run on wasm registry lifecycle paths"
    )
)]
mod peer_sync;
mod planner_stats;
mod prepared;
mod rbac;
mod seed;
pub mod state;
pub mod types;
mod use_graph;
mod use_graph_wire;
mod vertex_property_backfill;

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
fn my_role() -> Result<String, RouterError> {
    canister::my_role()
}

#[update]
fn admin_grant_role(args: types::GrantRoleArgs) -> Result<(), RouterError> {
    canister::admin_grant_role(args)
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
fn lookup_graph_id(graph_name: String) -> Result<gleaph_graph_kernel::entry::GraphId, RouterError> {
    canister::lookup_graph_id(graph_name)
}

#[query]
fn list_shards_for_graph(
    logical_graph_name: String,
) -> Result<Vec<types::ShardRegistryEntry>, RouterError> {
    canister::list_shards_for_graph(logical_graph_name)
}

#[query]
fn resolve_placement(
    vertex_id: types::GlobalVertexId,
) -> Result<types::VertexPlacement, RouterError> {
    canister::resolve_placement(vertex_id)
}

#[query]
fn resolve_global_at(
    shard_id: types::ShardId,
    local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
) -> Result<types::GlobalVertexId, RouterError> {
    canister::resolve_global_at(shard_id, local_vertex_id)
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
fn commit_vertex_placement(args: types::CommitVertexPlacementArgs) -> Result<(), RouterError> {
    canister::commit_vertex_placement(args)
}

#[update]
fn release_vertex_placement(args: types::ReleaseVertexPlacementArgs) -> Result<(), RouterError> {
    canister::release_vertex_placement(args)
}

/// Read-only GQL: composite query (calls index + graph query endpoints).
#[query(composite = true)]
async fn gql_query(
    query: String,
    params: Vec<u8>,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
    gql::gql_query(query, params).await
}

/// Update-path GQL entrypoint for non-DML escape hatches; DML requires `gql_execute_idempotent`.
#[update]
async fn gql_execute(query: String, params: Vec<u8>) -> Result<u64, RouterError> {
    gql::gql_execute(query, params).await
}

/// Idempotent GQL update. Reuse `client_mutation_key` only for retries of the same mutation.
#[update]
async fn gql_execute_idempotent(
    query: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<u64, RouterError> {
    gql::gql_execute_idempotent(query, params, client_mutation_key).await
}

/// Read-only GQL on the update path only (no composite-query savings; bypasses path check).
#[update]
async fn force_gql_execute(query: String, params: Vec<u8>) -> Result<u64, RouterError> {
    gql::force_gql_execute(query, params).await
}

#[update]
fn prepared_register(name: String, query: String) -> Result<(), RouterError> {
    prepared::prepared_register(name, query)
}

#[update]
fn prepared_drop(name: String) -> Result<(), RouterError> {
    prepared::prepared_drop(&name)
}

#[query(composite = true)]
async fn prepared_execute_query(
    name: String,
    params: Vec<u8>,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
    prepared::prepared_execute_query(name, params).await
}

#[update]
async fn prepared_execute_update(name: String, params: Vec<u8>) -> Result<u64, RouterError> {
    prepared::prepared_execute_update(name, params).await
}

#[update]
async fn prepared_execute_update_idempotent(
    name: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<u64, RouterError> {
    prepared::prepared_execute_update_idempotent(name, params, client_mutation_key).await
}

#[update]
async fn force_prepared_execute_update(name: String, params: Vec<u8>) -> Result<u64, RouterError> {
    prepared::force_prepared_execute_update(name, params).await
}

#[update]
async fn admin_set_indexed_vertex_property(
    logical_graph_name: String,
    vertex_label: String,
    property: String,
) -> Result<(), RouterError> {
    canister::admin_set_indexed_vertex_property(logical_graph_name, vertex_label, property).await
}

#[update]
async fn admin_set_indexed_edge_property(
    logical_graph_name: String,
    edge_label: String,
    property: String,
) -> Result<(), RouterError> {
    canister::admin_set_indexed_edge_property(logical_graph_name, edge_label, property).await
}

/// Advance label posting backfill for one graph shard (controller-only; call in a loop).
#[update]
async fn admin_label_backfill_step(
    args: types::AdminLabelBackfillStepArgs,
) -> Result<types::AdminLabelBackfillStepResult, RouterError> {
    canister::admin_label_backfill_step(args).await
}

/// List router-stable backfill cursors for all shards of a logical graph.
#[query]
fn admin_list_label_backfill_status(
    logical_graph_name: String,
) -> Result<Vec<types::LabelBackfillShardStatus>, RouterError> {
    canister::admin_list_label_backfill_status(logical_graph_name)
}

/// Advance vertex property posting backfill for one graph shard (controller-only; call in a loop).
#[update]
async fn admin_vertex_property_backfill_step(
    args: types::AdminVertexPropertyBackfillStepArgs,
) -> Result<types::AdminVertexPropertyBackfillStepResult, RouterError> {
    canister::admin_vertex_property_backfill_step(args).await
}

/// List router-stable vertex property backfill cursors for all shards of a logical graph.
#[query]
fn admin_list_vertex_property_backfill_status(
    logical_graph_name: String,
) -> Result<Vec<types::VertexPropertyBackfillShardStatus>, RouterError> {
    canister::admin_list_vertex_property_backfill_status(logical_graph_name)
}

/// Advance edge property posting backfill for one graph shard (controller-only; call in a loop).
#[update]
async fn admin_edge_backfill_step(
    args: types::AdminEdgeBackfillStepArgs,
) -> Result<types::AdminEdgeBackfillStepResult, RouterError> {
    canister::admin_edge_backfill_step(args).await
}

/// List router-stable edge backfill cursors for all shards of a logical graph.
#[query]
fn admin_list_edge_backfill_status(
    logical_graph_name: String,
) -> Result<Vec<types::EdgeBackfillShardStatus>, RouterError> {
    canister::admin_list_edge_backfill_status(logical_graph_name)
}

/// Advance label stats projection for one graph shard (controller-only; call in a loop).
#[update]
async fn admin_label_stats_projection_step(
    args: types::AdminLabelStatsProjectionStepArgs,
) -> Result<types::AdminLabelStatsProjectionStepResult, RouterError> {
    canister::admin_label_stats_projection_step(args).await
}

ic_cdk::export_candid!();
