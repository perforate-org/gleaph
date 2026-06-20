//! Canister request handlers for `gleaph-router`.

use crate::facade::auth;
use crate::facade::store::RouterStore;
use crate::index_ddl::IndexTarget;
use crate::init::RouterInitArgs;
use crate::state::RouterError;
use crate::types::{
    AdminEdgeBackfillStepArgs, AdminEdgeBackfillStepResult, AdminLabelBackfillStepArgs,
    AdminLabelBackfillStepResult, AdminLabelStatsProjectionStepArgs,
    AdminLabelStatsProjectionStepResult, AdminRegisterShardArgs,
    AdminVertexPropertyBackfillStepArgs, AdminVertexPropertyBackfillStepResult,
    EdgeBackfillShardStatus, EdgeLabelId, GrantRoleArgs, GraphRegistryEntry,
    LabelBackfillShardStatus, PropertyId, ShardId, ShardRegistryEntry, VertexLabelId,
    VertexPropertyBackfillShardStatus,
};
use candid::Principal;
use gleaph_gql_ic::graph_registry::GraphStatus;
use ic_cdk::api::msg_caller;

pub(crate) fn init(args: RouterInitArgs) {
    // Preflight: reject invalid bootstrap principals before clearing/writing any Router stable
    // state, so a failed init never mutates state and never depends on IC trap rollback.
    if let Err(e) =
        auth::validate_bootstrap_principals(args.issuing_principal, &args.initial_admins)
    {
        ic_cdk::trap(e.to_string());
    }
    RouterStore::new().init_from_args(&args);
    if let Err(e) = auth::bootstrap_canister_auth(args.issuing_principal, &args.initial_admins) {
        ic_cdk::trap(e.to_string());
    }
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

pub(crate) fn resolve_shard(
    logical_graph_name: String,
    shard_id: ShardId,
) -> Result<ShardRegistryEntry, RouterError> {
    let graph_id = RouterStore::new().resolve_graph_id(&logical_graph_name)?;
    RouterStore::new().resolve_shard(graph_id, shard_id)
}

pub(crate) fn lookup_graph_id(
    graph_name: String,
) -> Result<gleaph_graph_kernel::entry::GraphId, RouterError> {
    RouterStore::new().resolve_graph_id(&graph_name)
}

pub(crate) fn graph_element_id_encoding_key(
    logical_graph_name: String,
) -> Result<[u8; 16], RouterError> {
    auth::require_admin(&msg_caller())?;
    let graph_id = RouterStore::new().resolve_graph_id(&logical_graph_name)?;
    Ok(RouterStore::new()
        .graph_element_id_encoding_key(graph_id)?
        .0)
}

pub(crate) fn list_shards_for_graph(
    logical_graph_name: String,
) -> Result<Vec<ShardRegistryEntry>, RouterError> {
    RouterStore::new().list_shards_for_graph(&logical_graph_name)
}

/// Router-sourced snapshot of which properties are indexed for a graph (ADR 0023
/// D1/D3/P2). Graph shards consult this ephemerally per operation — including the
/// async maintenance tick that re-keys postings after compaction — so they never
/// persist derived index state across the upgrade boundary.
pub(crate) fn indexed_property_catalog(
    logical_graph_name: String,
) -> Result<gleaph_graph_kernel::index::IndexedPropertyCatalog, RouterError> {
    let graph_id = RouterStore::new().resolve_graph_id(&logical_graph_name)?;
    Ok(crate::index_catalog::graph_stats_for(graph_id).to_indexed_property_catalog())
}

pub(crate) fn lookup_vertex_label_id(
    logical_graph_name: String,
    name: String,
) -> Result<VertexLabelId, RouterError> {
    let graph_id = RouterStore::new().resolve_graph_id(&logical_graph_name)?;
    RouterStore::new().lookup_vertex_label_id(graph_id, &name)
}

pub(crate) fn lookup_edge_label_id(
    logical_graph_name: String,
    name: String,
) -> Result<EdgeLabelId, RouterError> {
    let graph_id = RouterStore::new().resolve_graph_id(&logical_graph_name)?;
    RouterStore::new().lookup_edge_label_id(graph_id, &name)
}

pub(crate) fn lookup_property_id(
    logical_graph_name: String,
    name: String,
) -> Result<PropertyId, RouterError> {
    let graph_id = RouterStore::new().resolve_graph_id(&logical_graph_name)?;
    RouterStore::new().lookup_property_id(graph_id, &name)
}

pub(crate) fn reverse_vertex_label_name(
    logical_graph_name: String,
    label_id: VertexLabelId,
) -> Result<String, RouterError> {
    let graph_id = RouterStore::new().resolve_graph_id(&logical_graph_name)?;
    RouterStore::new().reverse_vertex_label_name(graph_id, label_id)
}

pub(crate) fn reverse_edge_label_name(
    logical_graph_name: String,
    label_id: EdgeLabelId,
) -> Result<String, RouterError> {
    let graph_id = RouterStore::new().resolve_graph_id(&logical_graph_name)?;
    RouterStore::new().reverse_edge_label_name(graph_id, label_id)
}

pub(crate) fn reverse_property_name(
    logical_graph_name: String,
    property_id: PropertyId,
) -> Result<String, RouterError> {
    let graph_id = RouterStore::new().resolve_graph_id(&logical_graph_name)?;
    RouterStore::new().reverse_property_name(graph_id, property_id)
}

pub(crate) async fn admin_register_graph(entry: GraphRegistryEntry) -> Result<(), RouterError> {
    RouterStore::new()
        .admin_register_graph_with_random_key(msg_caller(), entry)
        .await
}

pub(crate) fn admin_update_graph_status(
    graph_name: String,
    status: GraphStatus,
    version: u64,
) -> Result<(), RouterError> {
    RouterStore::new().admin_update_graph_status(msg_caller(), &graph_name, status, version)
}

pub(crate) fn admin_unregister_graph(logical_graph_name: String) -> Result<(), RouterError> {
    RouterStore::new().admin_unregister_graph(msg_caller(), &logical_graph_name)
}

pub(crate) async fn admin_register_shard(args: AdminRegisterShardArgs) -> Result<(), RouterError> {
    RouterStore::new()
        .admin_register_shard(msg_caller(), args)
        .await
}

pub(crate) async fn admin_unregister_shard(
    logical_graph_name: String,
    shard_id: ShardId,
) -> Result<(), RouterError> {
    RouterStore::new()
        .admin_unregister_shard(msg_caller(), &logical_graph_name, shard_id)
        .await
}

/// Verify router registry denormalization invariants (regions 1–5, 15–16). `Ok(())`
/// means consistent; `Err(Internal(detail))` reports the first divergence. Read-only
/// oracle so registry consistency can be checked on demand, including across upgrades.
pub(crate) fn admin_check_registry_invariants() -> Result<(), RouterError> {
    auth::require_admin(&msg_caller())?;
    RouterStore::new()
        .check_registry_invariants()
        .map_err(RouterError::Internal)
}

pub(crate) fn admin_intern_vertex_label(
    logical_graph_name: String,
    name: String,
) -> Result<VertexLabelId, RouterError> {
    RouterStore::new().admin_intern_vertex_label(msg_caller(), &logical_graph_name, &name)
}

pub(crate) fn admin_intern_edge_label(
    logical_graph_name: String,
    name: String,
) -> Result<EdgeLabelId, RouterError> {
    RouterStore::new().admin_intern_edge_label(msg_caller(), &logical_graph_name, &name)
}

pub(crate) fn admin_intern_property(
    logical_graph_name: String,
    name: String,
) -> Result<PropertyId, RouterError> {
    RouterStore::new().admin_intern_property(msg_caller(), &logical_graph_name, &name)
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

pub(crate) async fn admin_vertex_property_backfill_step(
    args: AdminVertexPropertyBackfillStepArgs,
) -> Result<AdminVertexPropertyBackfillStepResult, RouterError> {
    let catalog = RouterStore::new()
        .resolve_graph_id(&args.logical_graph_name)
        .map(|graph_id| {
            crate::index_catalog::graph_stats_for(graph_id).to_indexed_property_catalog()
        })
        .unwrap_or_default();
    crate::vertex_property_backfill::admin_vertex_property_backfill_step(
        &RouterStore::new(),
        msg_caller(),
        args,
        move |graph, bargs| {
            crate::graph_client::backfill_vertex_property_postings(graph, bargs, catalog.clone())
        },
    )
    .await
}

pub(crate) fn admin_list_vertex_property_backfill_status(
    logical_graph_name: String,
) -> Result<Vec<VertexPropertyBackfillShardStatus>, RouterError> {
    crate::vertex_property_backfill::admin_list_vertex_property_backfill_status(
        &RouterStore::new(),
        msg_caller(),
        &logical_graph_name,
    )
}

pub(crate) async fn admin_edge_backfill_step(
    args: AdminEdgeBackfillStepArgs,
) -> Result<AdminEdgeBackfillStepResult, RouterError> {
    let catalog = RouterStore::new()
        .resolve_graph_id(&args.logical_graph_name)
        .map(|graph_id| {
            crate::index_catalog::graph_stats_for(graph_id).to_indexed_property_catalog()
        })
        .unwrap_or_default();
    crate::edge_backfill::admin_edge_backfill_step(
        &RouterStore::new(),
        msg_caller(),
        args,
        move |graph, bargs| {
            crate::graph_client::backfill_edge_property_postings(graph, bargs, catalog.clone())
        },
    )
    .await
}

pub(crate) fn admin_list_edge_backfill_status(
    logical_graph_name: String,
) -> Result<Vec<EdgeBackfillShardStatus>, RouterError> {
    crate::edge_backfill::admin_list_edge_backfill_status(
        &RouterStore::new(),
        msg_caller(),
        &logical_graph_name,
    )
}

pub(crate) async fn admin_label_stats_projection_step(
    args: AdminLabelStatsProjectionStepArgs,
) -> Result<AdminLabelStatsProjectionStepResult, RouterError> {
    crate::label_stats_projection::admin_label_stats_projection_step(
        &RouterStore::new(),
        msg_caller(),
        args,
        crate::graph_client::list_pending_label_stats_deltas,
        crate::graph_client::ack_label_stats_deltas_through,
    )
    .await
}

pub(crate) async fn admin_set_indexed_vertex_property(
    logical_graph_name: String,
    vertex_label: String,
    property: String,
) -> Result<(), RouterError> {
    use gleaph_graph_kernel::index::IndexedPropertyKind;

    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&logical_graph_name)?;
    crate::index_catalog::create_admin_compat_property_index(
        graph_id,
        IndexTarget {
            kind: IndexedPropertyKind::Vertex,
            label: vertex_label,
            property,
            edge_direction: None,
        },
    )
    .await
}

pub(crate) async fn admin_set_indexed_edge_property(
    logical_graph_name: String,
    edge_label: String,
    property: String,
) -> Result<(), RouterError> {
    use gleaph_gql::types::EdgeDirection;
    use gleaph_graph_kernel::index::IndexedPropertyKind;

    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&logical_graph_name)?;
    crate::index_catalog::create_admin_compat_property_index(
        graph_id,
        IndexTarget {
            kind: IndexedPropertyKind::Edge,
            label: edge_label,
            property,
            edge_direction: Some(EdgeDirection::AnyDirection),
        },
    )
    .await
}
