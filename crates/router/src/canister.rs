//! Canister request handlers for `gleaph-router`.

use crate::facade::auth;
use crate::facade::store::RouterStore;
use crate::init::RouterInitArgs;
use crate::state::RouterError;
use crate::types::{
    AdminLabelBackfillStepArgs, AdminLabelBackfillStepResult, AdminLabelTelemetryReplayStepArgs,
    AdminLabelTelemetryReplayStepResult, AdminPropertyBackfillStepArgs,
    AdminPropertyBackfillStepResult, AdminRegisterShardArgs, CommitVertexPlacementArgs,
    EdgeLabelId, GlobalVertexId, GrantRoleArgs, GraphRegistryEntry, LabelBackfillShardStatus,
    PropertyBackfillShardStatus, PropertyId, ReleaseVertexPlacementArgs, ShardId,
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

pub(crate) fn lookup_graph_id(
    graph_name: String,
) -> Result<gleaph_graph_kernel::entry::GraphId, RouterError> {
    RouterStore::new().resolve_graph_id(&graph_name)
}

pub(crate) fn list_shards_for_graph(
    logical_graph_name: String,
) -> Result<Vec<ShardRegistryEntry>, RouterError> {
    RouterStore::new().list_shards_for_graph(&logical_graph_name)
}

pub(crate) fn resolve_placement(vertex_id: GlobalVertexId) -> Result<VertexPlacement, RouterError> {
    RouterStore::new().resolve_placement(vertex_id)
}

pub(crate) fn resolve_global_at(
    shard_id: ShardId,
    local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
) -> Result<GlobalVertexId, RouterError> {
    RouterStore::new().resolve_global_at(shard_id, local_vertex_id)
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

pub(crate) async fn admin_label_backfill_step(
    args: AdminLabelBackfillStepArgs,
) -> Result<AdminLabelBackfillStepResult, RouterError> {
    crate::label_backfill::admin_label_backfill_step(
        &RouterStore::new(),
        msg_caller(),
        args,
        crate::graph_client::backfill_label_postings,
    )
    .await
}

pub(crate) fn admin_list_label_backfill_status(
    logical_graph_name: String,
) -> Result<Vec<LabelBackfillShardStatus>, RouterError> {
    crate::label_backfill::admin_list_label_backfill_status(
        &RouterStore::new(),
        msg_caller(),
        &logical_graph_name,
    )
}

pub(crate) async fn admin_property_backfill_step(
    args: AdminPropertyBackfillStepArgs,
) -> Result<AdminPropertyBackfillStepResult, RouterError> {
    crate::property_backfill::admin_property_backfill_step(
        &RouterStore::new(),
        msg_caller(),
        args,
        crate::graph_client::backfill_property_postings,
    )
    .await
}

pub(crate) fn admin_list_property_backfill_status(
    logical_graph_name: String,
) -> Result<Vec<PropertyBackfillShardStatus>, RouterError> {
    crate::property_backfill::admin_list_property_backfill_status(
        &RouterStore::new(),
        msg_caller(),
        &logical_graph_name,
    )
}

pub(crate) async fn admin_label_telemetry_replay_step(
    args: AdminLabelTelemetryReplayStepArgs,
) -> Result<AdminLabelTelemetryReplayStepResult, RouterError> {
    crate::label_telemetry_replay::admin_label_telemetry_replay_step(
        &RouterStore::new(),
        msg_caller(),
        args,
        crate::graph_client::list_pending_label_telemetry_events,
        crate::graph_client::ack_label_telemetry_event,
    )
    .await
}

pub(crate) async fn admin_set_indexed_vertex_property(
    logical_graph_name: String,
    property: String,
) -> Result<(), RouterError> {
    use gleaph_graph_kernel::index::IndexedPropertyKind;

    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&logical_graph_name)?;
    let property_id = store.lookup_property_id(&property)?;
    let newly_registered = crate::index_catalog::register_property_membership_if_absent(
        graph_id,
        IndexedPropertyKind::Vertex,
        property_id,
    );
    if !newly_registered {
        return Ok(());
    }
    crate::index_catalog::register_indexed_property_on_shards(
        graph_id,
        IndexedPropertyKind::Vertex,
        property_id,
    )
    .await
}

pub(crate) async fn admin_set_indexed_edge_property(
    logical_graph_name: String,
    property: String,
) -> Result<(), RouterError> {
    use gleaph_graph_kernel::index::IndexedPropertyKind;

    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&logical_graph_name)?;
    let property_id = store.lookup_property_id(&property)?;
    let newly_registered = crate::index_catalog::register_property_membership_if_absent(
        graph_id,
        IndexedPropertyKind::Edge,
        property_id,
    );
    if !newly_registered {
        return Ok(());
    }
    crate::index_catalog::register_indexed_property_on_shards(
        graph_id,
        IndexedPropertyKind::Edge,
        property_id,
    )
    .await
}

pub(crate) fn commit_vertex_placement(args: CommitVertexPlacementArgs) -> Result<(), RouterError> {
    RouterStore::new().commit_vertex_placement(msg_caller(), args)
}

pub(crate) fn release_vertex_placement(
    args: ReleaseVertexPlacementArgs,
) -> Result<(), RouterError> {
    RouterStore::new().release_vertex_placement(msg_caller(), args)
}
