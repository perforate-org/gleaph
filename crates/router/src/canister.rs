//! Canister request handlers for `gleaph-router`.

use crate::facade::store::RouterStore;
use crate::init::RouterInitArgs;
use crate::state::RouterError;
use crate::types::{
    AdminRegisterShardArgs, BeginVertexMigrationArgs, CommitVertexPlacementArgs, EdgeLabelId,
    FinishVertexMigrationArgs, GraphRegistryEntry, LogicalVertexId, PropertyId,
    ReleaseLogicalVertexArgs, ShardId, ShardRegistryEntry, VertexLabelId, VertexPlacement,
};
use candid::Principal;
use gleaph_gql_ic::graph_registry::GraphStatus;
use ic_cdk::api::msg_caller;

pub(crate) fn init(args: RouterInitArgs) {
    RouterStore::new().init_from_args(&args);
}

pub(crate) fn whoami() -> Principal {
    msg_caller()
}

pub(crate) fn resolve_graph(graph_name: String) -> Result<GraphRegistryEntry, RouterError> {
    RouterStore::new().resolve_graph(&graph_name, msg_caller())
}

pub(crate) fn resolve_shard(shard_id: ShardId) -> Result<ShardRegistryEntry, RouterError> {
    RouterStore::new().resolve_shard(shard_id)
}

pub(crate) fn list_shards_for_graph(
    logical_graph_name: String,
) -> Result<Vec<ShardRegistryEntry>, RouterError> {
    RouterStore::new().list_shards_for_graph(&logical_graph_name)
}

pub(crate) fn resolve_placement(
    logical_vertex_id: LogicalVertexId,
) -> Result<VertexPlacement, RouterError> {
    RouterStore::new().resolve_placement(logical_vertex_id)
}

pub(crate) fn resolve_logical_at(
    shard_id: ShardId,
    local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
) -> Result<LogicalVertexId, RouterError> {
    RouterStore::new().resolve_logical_at(shard_id, local_vertex_id)
}

pub(crate) fn lookup_vertex_label_id(name: String) -> Result<VertexLabelId, RouterError> {
    RouterStore::new().lookup_vertex_label_id(&name)
}

pub(crate) fn lookup_edge_label_id(name: String) -> Result<EdgeLabelId, RouterError> {
    RouterStore::new().lookup_edge_label_id(&name)
}

pub(crate) fn lookup_property_id(name: String) -> Result<PropertyId, RouterError> {
    RouterStore::new().lookup_property_id(&name)
}

pub(crate) fn reverse_vertex_label_name(label_id: VertexLabelId) -> Result<String, RouterError> {
    RouterStore::new().reverse_vertex_label_name(label_id)
}

pub(crate) fn reverse_edge_label_name(label_id: EdgeLabelId) -> Result<String, RouterError> {
    RouterStore::new().reverse_edge_label_name(label_id)
}

pub(crate) fn reverse_property_name(property_id: PropertyId) -> Result<String, RouterError> {
    RouterStore::new().reverse_property_name(property_id)
}

pub(crate) fn admin_register_graph(entry: GraphRegistryEntry) -> Result<(), RouterError> {
    RouterStore::new().admin_register_graph(msg_caller(), entry)
}

pub(crate) fn admin_update_graph_status(
    graph_name: String,
    status: GraphStatus,
    version: u64,
) -> Result<(), RouterError> {
    RouterStore::new().admin_update_graph_status(msg_caller(), &graph_name, status, version)
}

pub(crate) async fn admin_register_shard(args: AdminRegisterShardArgs) -> Result<(), RouterError> {
    RouterStore::new()
        .admin_register_shard(msg_caller(), args)
        .await
}

pub(crate) async fn admin_unregister_shard(shard_id: ShardId) -> Result<(), RouterError> {
    RouterStore::new()
        .admin_unregister_shard(msg_caller(), shard_id)
        .await
}

pub(crate) fn admin_intern_vertex_label(name: String) -> Result<VertexLabelId, RouterError> {
    RouterStore::new().admin_intern_vertex_label(msg_caller(), &name)
}

pub(crate) fn admin_intern_edge_label(name: String) -> Result<EdgeLabelId, RouterError> {
    RouterStore::new().admin_intern_edge_label(msg_caller(), &name)
}

pub(crate) fn admin_intern_property(name: String) -> Result<PropertyId, RouterError> {
    RouterStore::new().admin_intern_property(msg_caller(), &name)
}

pub(crate) fn allocate_logical_vertex_id() -> Result<LogicalVertexId, RouterError> {
    RouterStore::new().allocate_logical_vertex_id(msg_caller())
}

pub(crate) fn commit_vertex_placement(args: CommitVertexPlacementArgs) -> Result<(), RouterError> {
    RouterStore::new().commit_vertex_placement(msg_caller(), args)
}

pub(crate) fn begin_vertex_migration(args: BeginVertexMigrationArgs) -> Result<(), RouterError> {
    RouterStore::new().begin_vertex_migration(msg_caller(), args)
}

pub(crate) fn finish_vertex_migration(args: FinishVertexMigrationArgs) -> Result<(), RouterError> {
    RouterStore::new().finish_vertex_migration(msg_caller(), args)
}

pub(crate) fn release_logical_vertex_placement(
    args: ReleaseLogicalVertexArgs,
) -> Result<(), RouterError> {
    RouterStore::new().release_logical_vertex_placement(msg_caller(), args)
}
