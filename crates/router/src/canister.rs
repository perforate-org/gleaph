//! Canister request handlers for `gleaph-router`.

use crate::facade::auth;
use crate::facade::store::RouterStore;
use crate::index_ddl::IndexTarget;
use crate::init::RouterInitArgs;
use crate::state::RouterError;
use crate::types::{
    AdminAttachVectorIndexShardArgs, AdminEdgeBackfillStepArgs, AdminEdgeBackfillStepResult,
    AdminLabelBackfillStepArgs, AdminLabelBackfillStepResult, AdminLabelStatsProjectionStepArgs,
    AdminLabelStatsProjectionStepResult, AdminRegisterShardArgs, AdminSweepMutationKeysStepArgs,
    AdminSweepMutationKeysStepResult, AdminVectorIndexBackfillStepArgs,
    AdminVectorIndexBackfillStepResult, AdminVertexPropertyBackfillStepArgs,
    AdminVertexPropertyBackfillStepResult, EdgeBackfillShardStatus, EdgeLabelId, GrantRoleArgs,
    GraphRegistryEntry, LabelBackfillShardStatus, PropertyId, RegisterVectorIndexArgs,
    SetVectorIndexTargetArgs, ShardId, ShardRegistryEntry, VectorIndexActivationStateView,
    VectorIndexActivationStatus, VectorIndexInfo, VertexLabelId, VertexPropertyBackfillShardStatus,
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
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().resolve_shard(graph_id, shard_id)
}

pub(crate) fn lookup_graph_id(
    graph_name: String,
) -> Result<gleaph_graph_kernel::entry::GraphId, RouterError> {
    RouterStore::new().resolve_graph_id_authorized(&graph_name, msg_caller())
}

/// ADR 0029 Phase 4: pull-based status of a caller's federated mutation. Read-only; scoped
/// to the caller's own `client_mutation_key` under an authorized graph.
pub(crate) fn mutation_status(
    logical_graph_name: String,
    client_mutation_key: String,
) -> Result<crate::types::MutationStatus, RouterError> {
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id_authorized(&logical_graph_name, caller)?;
    let record = store
        .router_mutation_record(caller, graph_id, &client_mutation_key)
        .ok_or_else(|| {
            RouterError::InvalidArgument(
                "no mutation found for this client_mutation_key".to_string(),
            )
        })?;
    Ok(crate::types::MutationStatus::from_record(&record))
}

/// Test-only (`pocket-ic-e2e`): inject a projection-lagging federated saga so the autonomous
/// recovery driver's convergence can be exercised end-to-end. `mutation_id` must name a mutation
/// already committed on the graph's live shards (typically the token from a prior idempotent DML on
/// the same graph). See [`RouterStore::test_insert_projection_pending_record`]. Arms the recovery
/// timer so the injected non-terminal saga is picked up on the next tick.
#[cfg(feature = "pocket-ic-e2e")]
pub(crate) fn test_inject_projection_pending_saga(
    logical_graph_name: String,
    client_mutation_key: String,
    mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    row_count: u64,
) -> Result<(), RouterError> {
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id_authorized(&logical_graph_name, caller)?;
    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    store.test_insert_projection_pending_record(
        caller,
        graph_id,
        &client_mutation_key,
        mutation_id,
        row_count,
        &shards,
    )?;
    crate::recovery::arm_if_needed();
    Ok(())
}

/// Test-only (`pocket-ic-e2e`): declare a uniqueness constraint so the E2E suite can exercise the
/// full ADR 0030 write-path lifecycle end to end. Public `CREATE`/`DROP CONSTRAINT` DDL stays
/// `NotImplemented` (CREATE pending the publication decision, DROP pending a dedicated lifecycle
/// slice — ADR 0030 Revisions #14–#15; see [`crate::gql`]); this seam reaches the same
/// admin-authorized, declare-on-empty store path ([`RouterStore::create_unique_constraint`]) without
/// publishing the DDL. The constraint must be declared on a **brand-new** vertex label (declare-on-
/// empty), so call it before any vertex of `label` is inserted.
#[cfg(feature = "pocket-ic-e2e")]
pub(crate) fn test_declare_unique_constraint(
    logical_graph_name: String,
    constraint_name: String,
    label: String,
    property: String,
) -> Result<(), RouterError> {
    let caller = msg_caller();
    auth::require_admin(&caller)?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id_authorized(&logical_graph_name, caller)?;
    store.create_unique_constraint(graph_id, &constraint_name, false, &label, &property)
}

/// Test-only (`pocket-ic-e2e`): arm/clear an ADR 0030 write-path fault injection (admin-only).
#[cfg(feature = "pocket-ic-e2e")]
pub(crate) fn test_arm_fault(code: u8) -> Result<(), RouterError> {
    auth::require_admin(&msg_caller())?;
    let fault = crate::test_fault::fault_from_code(code)
        .ok_or_else(|| RouterError::InvalidArgument(format!("unknown fault code {code}")))?;
    crate::test_fault::arm(fault);
    Ok(())
}

/// Test-only (`pocket-ic-e2e`): force a `Reserved` reservation into `Reclaiming` (admin-only), so the
/// failure-injection suite can prove a same-`ClaimId` retry is fenced during a reclaim proof.
#[cfg(feature = "pocket-ic-e2e")]
pub(crate) fn test_force_reclaiming(
    logical_graph_name: String,
    label: String,
    property: String,
    value: String,
) -> Result<bool, RouterError> {
    let caller = msg_caller();
    auth::require_admin(&caller)?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id_authorized(&logical_graph_name, caller)?;
    store.test_force_reclaiming_text(graph_id, &label, &property, &value)
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
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().list_shards_for_graph_id(graph_id)
}

/// Router-sourced snapshot of which properties are indexed for a graph (ADR 0023
/// D1/D3/P2). Graph shards consult this ephemerally per operation — including the
/// async maintenance tick that re-keys postings after compaction — so they never
/// persist derived index state across the upgrade boundary.
pub(crate) fn indexed_property_catalog(
    logical_graph_name: String,
) -> Result<gleaph_graph_kernel::index::IndexedPropertyCatalog, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    Ok(crate::index_catalog::graph_stats_for(graph_id).to_indexed_property_catalog())
}

pub(crate) fn lookup_vertex_label_id(
    logical_graph_name: String,
    name: String,
) -> Result<VertexLabelId, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().lookup_vertex_label_id(graph_id, &name)
}

pub(crate) fn lookup_edge_label_id(
    logical_graph_name: String,
    name: String,
) -> Result<EdgeLabelId, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().lookup_edge_label_id(graph_id, &name)
}

pub(crate) fn lookup_property_id(
    logical_graph_name: String,
    name: String,
) -> Result<PropertyId, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().lookup_property_id(graph_id, &name)
}

pub(crate) fn reverse_vertex_label_name(
    logical_graph_name: String,
    label_id: VertexLabelId,
) -> Result<String, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().reverse_vertex_label_name(graph_id, label_id)
}

pub(crate) fn reverse_edge_label_name(
    logical_graph_name: String,
    label_id: EdgeLabelId,
) -> Result<String, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().reverse_edge_label_name(graph_id, label_id)
}

pub(crate) fn reverse_property_name(
    logical_graph_name: String,
    property_id: PropertyId,
) -> Result<String, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
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

/// Evict expired client-mutation idempotency records in a bounded, paginated pass.
/// Call repeatedly, feeding `next_cursor` back as `start_after`, until `done`.
pub(crate) fn admin_sweep_expired_client_mutation_keys(
    args: AdminSweepMutationKeysStepArgs,
) -> Result<AdminSweepMutationKeysStepResult, RouterError> {
    RouterStore::new().admin_sweep_expired_client_mutation_keys(
        msg_caller(),
        args.start_after,
        args.max_scan,
    )
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

pub(crate) fn admin_reset_backfill_claim(
    args: crate::types::AdminResetBackfillClaimArgs,
) -> Result<(), RouterError> {
    RouterStore::new().admin_reset_backfill_claim(msg_caller(), &args)
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

    crate::rbac::authorize_index_ddl(&msg_caller())?;
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

    crate::rbac::authorize_index_ddl(&msg_caller())?;
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

// --- derived vector index catalog (ADR 0031 Slice 3) ---

fn activation_state_view(
    state: crate::facade::stable::vector_index_catalog::VectorIndexActivationState,
) -> VectorIndexActivationStateView {
    use crate::facade::stable::vector_index_catalog::VectorIndexActivationState as S;
    match state {
        S::Registered => VectorIndexActivationStateView::Registered,
        S::DispatchBlocked => VectorIndexActivationStateView::DispatchBlocked,
        S::DispatchEnabled => VectorIndexActivationStateView::DispatchEnabled,
    }
}

fn vector_index_info(
    def: &crate::facade::stable::vector_index_catalog::VectorIndexDefRecord,
    dispatch_ready: bool,
) -> VectorIndexInfo {
    let effective = crate::facade::stable::vector_index_catalog::effective_activation_state(
        def.activation_state,
        dispatch_ready,
    );
    VectorIndexInfo {
        index_id: def.index_id,
        embedding_name_id: def.embedding_name_id.raw(),
        dims: def.dims,
        target: def.target.map(|t| t.canister),
        activation_state: activation_state_view(effective),
    }
}

pub(crate) fn admin_register_vector_index(
    args: RegisterVectorIndexArgs,
) -> Result<bool, RouterError> {
    use crate::facade::stable::{embedding_name_catalog, vector_index_catalog};
    use gleaph_graph_kernel::vector_index::{VectorEncoding, VectorIndexKind, VectorMetric};

    crate::rbac::authorize_index_ddl(&msg_caller())?;
    if args.embedding_name.is_empty() {
        return Err(RouterError::InvalidArgument(
            "embedding_name must not be empty".to_owned(),
        ));
    }
    if args.dims == 0 {
        return Err(RouterError::InvalidArgument("dims must be > 0".to_owned()));
    }
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&args.logical_graph_name)?;
    let target = args
        .target
        .map(|canister| vector_index_catalog::VectorIndexTarget { canister });
    // Preflight (conflict / if-not-exists no-op / anonymous-target rejection) BEFORE interning the
    // embedding name, so a rejected or no-op registration never allocates a durable EmbeddingNameId
    // (which would pollute the graph-scoped name catalog and could exhaust the u16 name space).
    if vector_index_catalog::preflight_register(
        graph_id,
        args.index_id,
        target,
        args.if_not_exists,
    )? == vector_index_catalog::RegisterPreflight::AlreadyExists
    {
        return Ok(false);
    }
    let embedding_name_id =
        embedding_name_catalog::intern_embedding_name(graph_id, &args.embedding_name)?;
    // Slice 3 supports exactly one variant of each physical parameter; the wire stays
    // algorithm-neutral and the catalog records the only supported shape.
    vector_index_catalog::register_vector_index(
        graph_id,
        args.index_id,
        embedding_name_id,
        VectorIndexKind::IvfFlat,
        VectorMetric::L2Squared,
        VectorEncoding::F32,
        args.dims,
        target,
        args.if_not_exists,
    )
}

pub(crate) fn admin_set_vector_index_target(
    args: SetVectorIndexTargetArgs,
) -> Result<(), RouterError> {
    use crate::facade::stable::vector_index_catalog::{self, VectorIndexTarget};

    crate::rbac::authorize_index_ddl(&msg_caller())?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&args.logical_graph_name)?;
    vector_index_catalog::set_vector_index_target(
        graph_id,
        args.index_id,
        VectorIndexTarget {
            canister: args.target,
        },
    )
}

pub(crate) fn list_vector_indexes(
    logical_graph_name: String,
) -> Result<Vec<VectorIndexInfo>, RouterError> {
    use crate::facade::stable::vector_index_catalog;

    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&logical_graph_name)?;
    let dispatch_ready = store.graph_vector_dispatch_ready(graph_id);
    Ok(vector_index_catalog::list_vector_indexes(graph_id)
        .iter()
        .map(|def| vector_index_info(def, dispatch_ready))
        .collect())
}

/// Inspect-only single-target resolution (ADR 0031 Slice 3). Returns the definition's resolved
/// target principal, rejecting a missing/unset/anonymous target. This surface is admin-visible only;
/// the target is never pushed to graph shards or consumed by any execution path in Slice 3.
pub(crate) fn resolve_vector_index_target(
    logical_graph_name: String,
    index_id: u32,
) -> Result<Principal, RouterError> {
    use crate::facade::stable::vector_index_catalog;

    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&logical_graph_name)?;
    vector_index_catalog::vector_index_target_for(graph_id, index_id)
}

pub(crate) fn vector_index_activation_status(
    logical_graph_name: String,
    index_id: u32,
) -> Result<VectorIndexActivationStatus, RouterError> {
    use crate::facade::stable::vector_index_catalog;

    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&logical_graph_name)?;
    let def = vector_index_catalog::get_vector_index(graph_id, index_id)
        .ok_or_else(|| RouterError::NotFound(format!("vector index {index_id}")))?;
    let global_enabled =
        crate::facade::stable::vector_activation::vector_dispatch_globally_enabled();
    let dispatch_ready = store.graph_vector_dispatch_ready(graph_id);
    let blocked_reason = vector_index_catalog::activation_block_reason(
        def.activation_state,
        global_enabled,
        dispatch_ready,
    )
    .map(|r| r.to_string());
    Ok(VectorIndexActivationStatus {
        index_id,
        activation_state: activation_state_view(vector_index_catalog::effective_activation_state(
            def.activation_state,
            dispatch_ready,
        )),
        blocked_reason,
    })
}

/// Admin vector-index backfill surface (ADR 0031 Slice 3). Validates the definition exists, then
/// **fails closed**: production backfill cannot run until delete-spanning incarnation fencing lands.
/// The production graph backfill endpoint/`graph_client` caller is deliberately deferred to the
/// activation/fencing slice (the test-only bounded worker is exercised directly in
/// `index::vertex_embedding_backfill`).
pub(crate) async fn admin_vector_index_backfill_step(
    args: AdminVectorIndexBackfillStepArgs,
) -> Result<AdminVectorIndexBackfillStepResult, RouterError> {
    use crate::facade::stable::vector_index_catalog;

    crate::rbac::authorize_index_ddl(&msg_caller())?;
    if args.max_vertices == 0 {
        return Err(RouterError::InvalidArgument(
            "max_vertices must be > 0".to_owned(),
        ));
    }
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&args.logical_graph_name)?;
    let def = vector_index_catalog::get_vector_index(graph_id, args.index_id)
        .ok_or_else(|| RouterError::NotFound(format!("vector index {}", args.index_id)))?;
    // The *requested* definition itself must be dispatch-enabled — not merely some sibling def of a
    // ready graph. A def with no target can never dispatch, so backfilling it would otherwise
    // populate other indexes via the graph-wide catalog. Fail closed before touching the shard.
    if def.target.is_none() {
        return Err(RouterError::Conflict(format!(
            "vector index {} has no target set",
            args.index_id
        )));
    }
    // Fail-closed on the dynamic gate (global flag + per-graph shard vector-attach to this target).
    let global_enabled =
        crate::facade::stable::vector_activation::vector_dispatch_globally_enabled();
    let dispatch_ready = store.graph_vector_dispatch_ready(graph_id);
    if let Some(reason) = vector_index_catalog::activation_block_reason(
        def.activation_state,
        global_enabled,
        dispatch_ready,
    ) {
        return Err(RouterError::VectorDispatchActivationBlocked(reason));
    }
    // Scope the worker to the requested index's embedding spec only, so a per-index backfill cannot
    // populate sibling indexes that share this (ready) graph.
    let catalog =
        vector_index_catalog::to_indexed_embedding_catalog_for_index(graph_id, args.index_id, true);
    let shard = store.resolve_shard(graph_id, args.shard_id)?;
    let result = crate::graph_client::backfill_vertex_embeddings(
        shard.graph_canister,
        gleaph_graph_kernel::federation::EmbeddingBackfillArgs {
            start_vertex_id: args.start_vertex_id,
            max_vertices: args.max_vertices,
        },
        catalog,
    )
    .await
    .map_err(RouterError::Internal)?;
    Ok(AdminVectorIndexBackfillStepResult {
        shard_id: args.shard_id,
        next_vertex_id: result.next_vertex_id,
        vertices_processed: result.vertices_processed,
        embeddings_synced: result.embeddings_synced,
        done: result.done,
    })
}

/// Admin (ADR 0031 Slice 4): flip the global vector-dispatch activation flag. `false` keeps
/// production dispatch/backfill fail-closed across all graphs; reversible.
pub(crate) fn admin_set_vector_dispatch_activation(enabled: bool) -> Result<(), RouterError> {
    crate::rbac::authorize_vector_activation(&msg_caller())?;
    crate::facade::stable::vector_activation::set_vector_dispatch_globally_enabled(enabled);
    Ok(())
}

/// Reads the global vector-dispatch activation flag (ADR 0031 Slice 4).
pub(crate) fn vector_dispatch_activation_enabled() -> bool {
    crate::facade::stable::vector_activation::vector_dispatch_globally_enabled()
}

/// Admin (ADR 0031 Slice 4): wire (or retrofit) a derived vector-index target onto an
/// already-registered shard and drive the attach handshake (graph-local routing → vector attach →
/// durable readiness bit). Idempotent; enforces one vector-index target per graph.
pub(crate) async fn admin_attach_vector_index_shard(
    args: AdminAttachVectorIndexShardArgs,
) -> Result<(), RouterError> {
    let caller = msg_caller();
    crate::rbac::authorize_vector_activation(&caller)?;
    RouterStore::new()
        .admin_attach_vector_index_shard(caller, args)
        .await
}
