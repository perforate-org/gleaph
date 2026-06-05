//! Canister request handlers for `gleaph-router`.

use crate::facade::auth;
use crate::facade::store::RouterStore;
use crate::init::RouterInitArgs;
use crate::state::RouterError;
use crate::types::{
    AdminRegisterShardArgs, CommitVertexPlacementArgs, EdgeLabelId, GrantRoleArgs,
    GraphRegistryEntry, LogicalVertexId, PropertyId, ReleaseLogicalVertexArgs, ShardId,
    ShardRegistryEntry, VertexLabelId, VertexPlacement,
};
use candid::Principal;
use gleaph_gql_ic::graph_registry::GraphStatus;
use ic_cdk::api::msg_caller;

pub(crate) fn init(args: RouterInitArgs) {
    RouterStore::new().init_from_args(&args);
    auth::bootstrap_canister_auth(args.issuing_principal, &args.initial_admins);
}

pub(crate) fn whoami() -> Principal {
    msg_caller()
}

pub(crate) fn my_role() -> Result<String, RouterError> {
    Ok(auth::caller_role(&msg_caller()).to_string())
}

pub(crate) fn admin_grant_role(args: GrantRoleArgs) -> Result<(), RouterError> {
    let role = auth::parse_role(&args.role).map_err(RouterError::InvalidArgument)?;
    auth::admin_upsert_principal(&msg_caller(), args.target, role, args.manager_caps).map_err(|e| {
        if e.contains("required") {
            RouterError::Forbidden
        } else {
            RouterError::InvalidArgument(e)
        }
    })
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

pub(crate) fn admin_set_indexed_vertex_property(
    logical_graph_name: String,
    property: String,
) -> Result<(), RouterError> {
    use crate::facade::stable::ROUTER_INDEXED_PROPERTIES;
    use crate::planner_stats::RouterGraphStats;
    ROUTER_INDEXED_PROPERTIES.with_borrow_mut(|m| {
        let entry = m
            .entry(logical_graph_name)
            .or_insert_with(RouterGraphStats::default);
        *entry = entry.clone().with_indexed_vertex_property(property);
    });
    Ok(())
}

pub(crate) fn allocate_logical_vertex_id() -> Result<LogicalVertexId, RouterError> {
    RouterStore::new().allocate_logical_vertex_id(msg_caller())
}

pub(crate) fn commit_vertex_placement(args: CommitVertexPlacementArgs) -> Result<(), RouterError> {
    RouterStore::new().commit_vertex_placement(msg_caller(), args)
}

pub(crate) fn release_logical_vertex_placement(
    args: ReleaseLogicalVertexArgs,
) -> Result<(), RouterError> {
    RouterStore::new().release_logical_vertex_placement(msg_caller(), args)
}
