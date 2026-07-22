//! Canister request handlers for `gleaph-router`.

use crate::facade::auth;
use crate::facade::store::RouterStore;
use crate::index_ddl::IndexTarget;
use crate::init::{RouterInitArgs, RouterUpgradeArgs};
use crate::state::RouterError;
use crate::types::{
    AdminAttachVectorIndexShardArgs, AdminEdgeBackfillStepArgs, AdminEdgeBackfillStepResult,
    AdminLabelBackfillStepArgs, AdminLabelBackfillStepResult, AdminLabelStatsProjectionStepArgs,
    AdminLabelStatsProjectionStepResult, AdminRegisterShardArgs, AdminSweepMutationKeysStepArgs,
    AdminSweepMutationKeysStepResult, AdminVectorIndexBackfillStepArgs,
    AdminVectorIndexBackfillStepResult, AdminVertexPropertyBackfillStepArgs,
    AdminVertexPropertyBackfillStepResult, EdgeBackfillShardStatus, EdgeLabelId, GrantRoleArgs,
    GraphBatchInstrLogPage, GraphRegistryEntry, GraphStableMemoryStats, LabelBackfillShardStatus,
    PropertyId, RegisterVectorIndexArgs, RouterVectorSearchRequest, SetVectorIndexTargetArgs,
    SetVectorMaintenancePolicyArgs, ShardId, ShardRegistryEntry, VectorIndexActivationStateView,
    VectorIndexActivationStatus, VectorIndexInfo, VectorMaintenancePolicyView,
    VectorMaintenanceStatusView, VectorMaintenanceStepOutcome, VertexLabelId,
    VertexPropertyBackfillShardStatus,
};
#[cfg(test)]
use candid::Decode;
use candid::Principal;
use gleaph_gql_ic::graph_registry::GraphStatus;
use gleaph_graph_kernel::vector_index::{
    VectorCentroidCacheStatus, VectorMaintenancePolicy, VectorMaintenanceRecommendation,
    VectorMaintenanceState, VectorPartitionHealthStep, VectorPartitionHealthSummary,
    VectorPartitionPageHealth, VectorRebuildStatus, VectorSlabStats, VectorSlabStatsStep,
};
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
    if let Err(e) = crate::init::validate_provision_principal(&args.provision_canister) {
        ic_cdk::trap(format!("init: {e}"));
    }
    crate::provisioning::config::set(args.provision_canister);
    crate::facade::stable::provision_config::save_provision_runtime_config(
        &crate::provisioning::config::ProvisionRuntimeConfig {
            provision_canister: args.provision_canister,
        },
    );
}

pub(crate) fn post_upgrade(args: RouterUpgradeArgs) {
    let durable = crate::facade::stable::provision_config::load_provision_runtime_config();
    let provision_canister =
        match resolve_provision_canister_for_upgrade(args.provision_canister, &durable) {
            Ok(p) => p,
            Err(e) => ic_cdk::trap(format!("post_upgrade: {e}")),
        };
    crate::provisioning::config::set(provision_canister);

    // Timers do not survive an upgrade; re-arm the recovery driver so non-terminal sagas
    // persisted across the upgrade still converge (ADR 0029 Phase 4).
    crate::recovery::arm_if_needed();
}

/// Decode Router upgrade args from Candid bytes.
///
/// Empty arg data is accepted as the stable "preserve durable configuration" form.
/// A non-empty payload must decode as [`RouterUpgradeArgs`]; anything else traps so
/// an operator cannot accidentally feed init args into an upgrade (ADR 0039).
#[cfg(test)]
pub(crate) fn decode_upgrade_args(arg_data: &[u8]) -> Option<RouterUpgradeArgs> {
    if arg_data.is_empty() {
        return None;
    }
    match candid::Decode!(arg_data, RouterUpgradeArgs) {
        Ok(args) => Some(args),
        Err(_) => ic_cdk::trap("post_upgrade: invalid upgrade args"),
    }
}

pub(crate) fn resolve_provision_canister_for_upgrade(
    override_arg: Option<Principal>,
    durable: &crate::provisioning::config::ProvisionRuntimeConfig,
) -> Result<Option<Principal>, &'static str> {
    // The durable ROUTER_PROVISION_CONFIG stable region is the SSOT for the provision-canister
    // binding. Upgrade args with `provision_canister: Some(p)` are an explicit operator override;
    // `None` means "preserve the durable binding". An invalid override is rejected with an
    // error and the durable binding is preserved.
    match override_arg {
        Some(p) => {
            crate::init::validate_provision_principal(&Some(p))?;
            crate::facade::stable::provision_config::save_provision_runtime_config(
                &crate::provisioning::config::ProvisionRuntimeConfig {
                    provision_canister: Some(p),
                },
            );
            Ok(Some(p))
        }
        None => Ok(durable.provision_canister),
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

/// Test-only (`pocket-ic-e2e`): arm/clear a Router write-path fault injection (admin-only).
#[cfg(feature = "pocket-ic-e2e")]
pub(crate) fn test_arm_fault(code: u8) -> Result<(), RouterError> {
    auth::require_admin(&msg_caller())?;
    let fault = crate::test_fault::fault_from_code(code)
        .ok_or_else(|| RouterError::InvalidArgument(format!("unknown fault code {code}")))?;
    crate::test_fault::arm(fault);
    Ok(())
}

#[cfg(feature = "pocket-ic-e2e")]
pub(crate) fn test_typed_batch_trace() -> Result<String, RouterError> {
    auth::require_admin(&msg_caller())?;
    Ok(crate::test_fault::typed_batch_trace())
}

#[cfg(feature = "pocket-ic-e2e")]
pub(crate) fn test_typed_batch_prepare_count() -> Result<u64, RouterError> {
    auth::require_admin(&msg_caller())?;
    Ok(crate::test_fault::typed_batch_prepare_count())
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

pub(crate) async fn admin_refresh_shard_execution_capabilities(
    logical_graph_name: String,
    shard_id: ShardId,
) -> Result<bool, RouterError> {
    RouterStore::new()
        .admin_refresh_shard_execution_capabilities(msg_caller(), &logical_graph_name, shard_id)
        .await
}

pub(crate) fn admin_clear_shard_execution_capabilities(
    logical_graph_name: String,
    shard_id: ShardId,
) -> Result<(), RouterError> {
    RouterStore::new().admin_clear_shard_execution_capabilities(
        msg_caller(),
        &logical_graph_name,
        shard_id,
    )
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
        metric: def.metric,
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
        args.metric.unwrap_or(VectorMetric::L2Squared),
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
    vector_index_catalog::assert_vector_search_dispatch_ready(graph_id, &store, &def)?;
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

/// Admin (plan 0048): ingest one finite F32 vertex embedding through Router into the owning
/// Graph shard. Resolves the opaque graph-scoped vertex id, validates the registered embedding
/// name/dimensions/finiteness, and dispatches a single canonical write. Returns the canonical
/// embedding version and an explicit projection outcome (`Applied` or `DeferredForRepair`).
pub(crate) async fn admin_ingest_vertex_embedding(
    args: crate::types::AdminIngestVertexEmbeddingArgs,
) -> Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, RouterError> {
    crate::rbac::authorize_index_ddl(&msg_caller())?;

    let store = RouterStore::new();
    let (graph_canister, ingestion_args) = resolve_vertex_embedding_ingestion(&args, &store)?;

    crate::graph_client::ingest_vertex_embedding(graph_canister, ingestion_args)
        .await
        .map_err(RouterError::Internal)
}

/// Maximum vertex-embedding ingestion items dispatched in a single Router→Graph inter-canister
/// call. The bound keeps the encoded Candid message well under the 2 MiB ingress/inter-canister
/// message limit and stays below the IC update-call instruction budget for the canonical write +
/// vector-index flush work performed inside the Graph shard. (Social-demo seed: ~71 items.)
const ADMIN_INGEST_VERTEX_EMBEDDING_BATCH_CHUNK: usize = 1_024;

/// Admin (plan 0048 extension): ingest a batch of finite F32 vertex embeddings through Router into
/// the owning Graph shard(s). Items are validated up front, grouped by target graph canister, and
/// sent in bounded chunks so a social-demo seed needs one Router→Graph call and one Graph→Vector
/// call instead of one call per embedding. Returns per-item results in the same order as `items`.
pub(crate) async fn admin_ingest_vertex_embedding_batch(
    args: crate::types::AdminIngestVertexEmbeddingBatchArgs,
) -> Result<
    Vec<Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, String>>,
    RouterError,
> {
    crate::rbac::authorize_index_ddl(&msg_caller())?;

    if args.items.is_empty() {
        return Ok(Vec::new());
    }

    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&args.logical_graph_name)?;
    let key = store.graph_element_id_encoding_key(graph_id)?;
    let live_shards = store.list_live_shards_for_graph_id(graph_id)?;

    use crate::facade::stable::{embedding_name_catalog, vector_index_catalog};
    use gleaph_graph_kernel::federation::{EncodedVertexId, decode_global_vertex_id};
    use gleaph_graph_kernel::vector_index::{IndexedEmbeddingSpec, VertexEmbeddingIngestionArgs};

    let name_id = embedding_name_catalog::lookup_embedding_name_id(graph_id, &args.embedding_name)
        .ok_or_else(|| {
            RouterError::NotFound(format!(
                "embedding name {} is not registered for this graph",
                args.embedding_name
            ))
        })?;
    let def = vector_index_catalog::get_vector_index_by_embedding_name_id(graph_id, name_id)
        .ok_or_else(|| {
            RouterError::NotFound(format!(
                "no vector index registered for embedding name {}",
                args.embedding_name
            ))
        })?;

    if def.encoding != gleaph_graph_kernel::vector_index::VectorEncoding::F32 {
        return Err(RouterError::InvalidArgument(format!(
            "encoding {:?} is not supported for ingestion; only F32 is accepted",
            def.encoding
        )));
    }

    let spec = IndexedEmbeddingSpec {
        embedding_name_id: name_id.raw(),
        index_id: def.index_id,
        kind: def.kind,
        metric: def.metric,
        encoding: def.encoding,
        dims: def.dims,
    };

    let item_count = args.items.len();

    // Resolve each item to its target graph canister and group by canister.
    type Grouped =
        std::collections::BTreeMap<candid::Principal, Vec<(VertexEmbeddingIngestionArgs, usize)>>;
    let mut by_canister: Grouped = Grouped::new();
    for (item_index, item) in args.items.into_iter().enumerate() {
        if item.encoded_vertex_id.len() != gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES
        {
            return Err(RouterError::InvalidArgument(format!(
                "encoded_vertex_id must be exactly {} bytes",
                gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES
            )));
        }
        if item.values.len() != def.dims as usize {
            return Err(RouterError::InvalidArgument(format!(
                "values length {} does not match vector index dims {}",
                item.values.len(),
                def.dims
            )));
        }
        if item.values.iter().copied().any(|v| !v.is_finite()) {
            return Err(RouterError::InvalidArgument(
                "values must be finite".to_string(),
            ));
        }

        let encoded_bytes: [u8; gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES] =
            item.encoded_vertex_id.as_slice().try_into().map_err(|_| {
                RouterError::InvalidArgument("encoded_vertex_id conversion failed".to_string())
            })?;
        let global_id = decode_global_vertex_id(&key, EncodedVertexId(encoded_bytes));
        let shard = live_shards
            .iter()
            .find(|s| s.shard_id == global_id.shard_id)
            .ok_or(RouterError::ShardNotRegistered)?;

        by_canister.entry(shard.graph_canister).or_default().push((
            VertexEmbeddingIngestionArgs {
                local_vertex_id: global_id.local_vertex_id,
                spec,
                values: item.values,
            },
            item_index,
        ));
    }

    let mut results: Vec<
        Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, String>,
    > = Vec::with_capacity(item_count);
    results.resize(item_count, Err("not dispatched".to_string()));

    for (graph_canister, mut group) in by_canister {
        group.sort_by_key(|(_, original_index)| *original_index);
        for chunk in group.chunks(ADMIN_INGEST_VERTEX_EMBEDDING_BATCH_CHUNK) {
            let chunk_args: Vec<VertexEmbeddingIngestionArgs> =
                chunk.iter().map(|(arg, _)| arg.clone()).collect();
            let chunk_results =
                crate::graph_client::ingest_vertex_embedding_batch(graph_canister, chunk_args)
                    .await
                    .map_err(RouterError::Internal)?;
            if chunk_results.len() != chunk.len() {
                return Err(RouterError::Internal(format!(
                    "graph returned {} results for {} ingestion args",
                    chunk_results.len(),
                    chunk.len()
                )));
            }
            for ((_, original_index), result) in chunk.iter().zip(chunk_results) {
                results[*original_index] = result;
            }
        }
    }

    Ok(results)
}

fn resolve_vertex_embedding_ingestion(
    args: &crate::types::AdminIngestVertexEmbeddingArgs,
    store: &RouterStore,
) -> Result<
    (
        candid::Principal,
        gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionArgs,
    ),
    RouterError,
> {
    use crate::facade::stable::{embedding_name_catalog, vector_index_catalog};
    use gleaph_graph_kernel::federation::{EncodedVertexId, decode_global_vertex_id};

    if args.embedding_name.is_empty() {
        return Err(RouterError::InvalidArgument(
            "embedding_name must not be empty".to_owned(),
        ));
    }
    if args.values.is_empty() {
        return Err(RouterError::InvalidArgument(
            "values must not be empty".to_owned(),
        ));
    }
    if args.values.iter().copied().any(|v| !v.is_finite()) {
        return Err(RouterError::InvalidArgument(
            "values must be finite".to_owned(),
        ));
    }
    if args.encoded_vertex_id.len() != gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "encoded_vertex_id must be exactly {} bytes",
            gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES
        )));
    }

    let graph_id = store.resolve_graph_id(&args.logical_graph_name)?;
    let key = store.graph_element_id_encoding_key(graph_id)?;
    let encoded_bytes: [u8; gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES] =
        args.encoded_vertex_id.as_slice().try_into().map_err(|_| {
            RouterError::InvalidArgument("encoded_vertex_id conversion failed".to_owned())
        })?;
    let global_id = decode_global_vertex_id(&key, EncodedVertexId(encoded_bytes));

    let live_shards = store.list_live_shards_for_graph_id(graph_id)?;
    let shard = live_shards
        .into_iter()
        .find(|s| s.shard_id == global_id.shard_id)
        .ok_or(RouterError::ShardNotRegistered)?;

    let name_id = embedding_name_catalog::lookup_embedding_name_id(graph_id, &args.embedding_name)
        .ok_or_else(|| {
            RouterError::NotFound(format!(
                "embedding name {} is not registered for this graph",
                args.embedding_name
            ))
        })?;
    let def = vector_index_catalog::get_vector_index_by_embedding_name_id(graph_id, name_id)
        .ok_or_else(|| {
            RouterError::NotFound(format!(
                "no vector index registered for embedding name {}",
                args.embedding_name
            ))
        })?;

    if args.values.len() != def.dims as usize {
        return Err(RouterError::InvalidArgument(format!(
            "values length {} does not match vector index dims {}",
            args.values.len(),
            def.dims
        )));
    }
    if def.encoding != gleaph_graph_kernel::vector_index::VectorEncoding::F32 {
        return Err(RouterError::InvalidArgument(format!(
            "encoding {:?} is not supported for ingestion; only F32 is accepted",
            def.encoding
        )));
    }

    let spec = gleaph_graph_kernel::vector_index::IndexedEmbeddingSpec {
        embedding_name_id: name_id.raw(),
        index_id: def.index_id,
        kind: def.kind,
        metric: def.metric,
        encoding: def.encoding,
        dims: def.dims,
    };

    Ok((
        shard.graph_canister,
        gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionArgs {
            local_vertex_id: global_id.local_vertex_id,
            spec,
            values: args.values.clone(),
        },
    ))
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

/// Public read-only exact `ivf_flat` vector search (ADR 0031 Slice 5). Resolves the graph/index to
/// its single activated target and fails closed unless the Slice 4 activation gate is satisfied,
/// keeping the public read path aligned with dispatch readiness. The vector canister is
/// router-guarded, so this Router surface is the only public entry.
pub(crate) async fn vector_search(
    req: RouterVectorSearchRequest,
) -> Result<gleaph_graph_kernel::vector_index::VectorSearchResult, RouterError> {
    use crate::facade::stable::vector_index_catalog;
    use gleaph_graph_kernel::vector_index::{MAX_VECTOR_SEARCH_TOP_K, VectorSearchRequest};

    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&req.logical_graph_name)?;
    let def = vector_index_catalog::get_vector_index(graph_id, req.index_id)
        .ok_or_else(|| RouterError::NotFound(format!("vector index {}", req.index_id)))?;
    // Prevalidate the public request against the Router-owned definition so user mistakes surface as
    // `InvalidArgument`, not as an opaque `Internal` from the downstream vector canister.
    if req.top_k == 0 || req.top_k > MAX_VECTOR_SEARCH_TOP_K {
        return Err(RouterError::InvalidArgument(format!(
            "top_k must be in 1..={MAX_VECTOR_SEARCH_TOP_K}"
        )));
    }
    if req.dims != def.dims {
        return Err(RouterError::InvalidArgument(format!(
            "query dims {} disagree with vector index {} dims {}",
            req.dims, req.index_id, def.dims
        )));
    }
    let expected_bytes = def.encoding.stride_bytes(def.dims) as usize;
    if req.query.len() != expected_bytes {
        return Err(RouterError::InvalidArgument(format!(
            "query byte length {} does not match dims*stride {}",
            req.query.len(),
            expected_bytes
        )));
    }
    let target = def
        .target
        .ok_or_else(|| {
            RouterError::Conflict(format!("vector index {} has no target set", req.index_id))
        })?
        .canister;
    // Fail closed on the dynamic gate (global flag + per-graph shard vector-attach to this target).
    vector_index_catalog::assert_vector_search_dispatch_ready(graph_id, &store, &def)?;
    let search = VectorSearchRequest {
        index_id: req.index_id,
        query: req.query,
        encoding: def.encoding,
        dims: req.dims,
        metric: def.metric,
        top_k: req.top_k,
        candidate_subjects: None,
    };
    crate::vector_sync::vector_search(target, search)
        .await
        .map_err(RouterError::Internal)
}

// --- ADR 0031 Slice 10: Router-forwarded vector maintenance surface ---
//
// All forwards are Admin-only (`authorize_vector_maintenance`) and fail closed unless the target is
// resolved, non-anonymous, and the per-graph dispatch gate is satisfied. Reads are exposed as
// composite queries, mutators/drivers as updates. The vector canister stays router-guarded, so these
// Router surfaces are the only operator entry points.

/// Resolves the vector target for one `(graph, index_id)` with the full fail-closed gate: graph
/// exists, the definition exists and is targeted to a non-anonymous canister, and the per-graph
/// dispatch activation gate is satisfied.
fn resolve_vector_maintenance_target(
    graph_name: &str,
    index_id: u32,
) -> Result<Principal, RouterError> {
    use crate::facade::stable::{vector_activation, vector_index_catalog};
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(graph_name)?;
    let def = vector_index_catalog::get_vector_index(graph_id, index_id)
        .ok_or_else(|| RouterError::NotFound(format!("vector index {index_id}")))?;
    let target = def
        .target
        .ok_or_else(|| RouterError::Conflict(format!("vector index {index_id} has no target set")))?
        .canister;
    if target == Principal::anonymous() {
        return Err(RouterError::Conflict(format!(
            "vector index {index_id} target is the anonymous principal"
        )));
    }
    let global_enabled = vector_activation::vector_dispatch_globally_enabled();
    let dispatch_ready = store.graph_vector_dispatch_ready(graph_id);
    if let Some(reason) = vector_index_catalog::activation_block_reason(
        def.activation_state,
        global_enabled,
        dispatch_ready,
    ) {
        return Err(RouterError::VectorDispatchActivationBlocked(reason));
    }
    Ok(target)
}

/// Resolves the graph's single vector target for graph-scoped maintenance ops (slab stats, whole-cache
/// status/clear) with the same fail-closed dispatch gate. Uses the one-target-per-graph invariant.
fn resolve_vector_graph_target(graph_name: &str) -> Result<Principal, RouterError> {
    use crate::facade::stable::vector_index_catalog::VectorIndexActivationState;
    use crate::facade::stable::{vector_activation, vector_index_catalog};
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(graph_name)?;
    let target = vector_index_catalog::graph_single_target(graph_id).ok_or_else(|| {
        RouterError::Conflict(format!("graph {graph_name} has no vector index target set"))
    })?;
    if target == Principal::anonymous() {
        return Err(RouterError::Conflict(format!(
            "graph {graph_name} vector target is the anonymous principal"
        )));
    }
    let global_enabled = vector_activation::vector_dispatch_globally_enabled();
    let dispatch_ready = store.graph_vector_dispatch_ready(graph_id);
    // The graph has a target (checked above), so the static state is DispatchBlocked; the gate then
    // requires the global flag + all shards vector-attached.
    if let Some(reason) = vector_index_catalog::activation_block_reason(
        VectorIndexActivationState::DispatchBlocked,
        global_enabled,
        dispatch_ready,
    ) {
        return Err(RouterError::VectorDispatchActivationBlocked(reason));
    }
    Ok(target)
}

pub(crate) async fn admin_vector_partition_health(
    graph_name: String,
    index_id: u32,
) -> Result<VectorPartitionHealthSummary, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_partition_health(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

/// Admin-only physical stable-memory inventory for every shard in a graph.
pub(crate) async fn admin_graph_stable_memory_stats(
    graph_name: String,
) -> Result<Vec<GraphStableMemoryStats>, RouterError> {
    crate::rbac::authorize_stable_memory_diagnostics(&msg_caller())?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&graph_name)?;
    let shards = store.list_shards_for_graph_id(graph_id)?;
    let mut stats = Vec::with_capacity(shards.len());
    for shard in shards {
        let memory = crate::graph_client::admin_stable_memory_stats(shard.graph_canister)
            .await
            .map_err(RouterError::Internal)?;
        stats.push(GraphStableMemoryStats {
            shard_id: shard.shard_id,
            graph_canister: shard.graph_canister,
            memory,
        });
    }
    Ok(stats)
}

/// Admin-only batch-instrumentation log proxy: one page per shard in the named graph.
#[allow(dead_code)]
pub(crate) async fn admin_graph_batch_instr_log(
    graph_name: String,
    offset: u32,
    limit: u32,
) -> Result<Vec<GraphBatchInstrLogPage>, RouterError> {
    crate::rbac::authorize_stable_memory_diagnostics(&msg_caller())?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&graph_name)?;
    let shards = store.list_shards_for_graph_id(graph_id)?;
    let mut pages = Vec::with_capacity(shards.len());
    for shard in shards {
        let lines =
            crate::graph_client::admin_take_batch_instr_log(shard.graph_canister, offset, limit)
                .await
                .map_err(RouterError::Internal)?;
        pages.push(GraphBatchInstrLogPage {
            shard_id: shard.shard_id,
            graph_canister: shard.graph_canister,
            lines,
        });
    }
    Ok(pages)
}

pub(crate) async fn admin_vector_partition_health_step(
    graph_name: String,
    index_id: u32,
    cursor: Option<Vec<u8>>,
    max_pages: u32,
) -> Result<VectorPartitionHealthStep, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_partition_health_step(
        target, index_id, cursor, max_pages,
    )
    .await
    .map_err(RouterError::Internal)
}

pub(crate) async fn admin_vector_rebuild_status(
    graph_name: String,
    index_id: u32,
) -> Result<VectorRebuildStatus, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_rebuild_status(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

pub(crate) async fn admin_vector_slab_stats(
    graph_name: String,
    index_id: Option<u32>,
) -> Result<VectorSlabStats, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_graph_target(&graph_name)?;
    crate::vector_sync::forward_admin_vector_slab_stats(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

pub(crate) async fn admin_vector_slab_stats_step(
    graph_name: String,
    cursor: Option<Vec<u8>>,
    max_pages: u32,
    index_id: Option<u32>,
) -> Result<VectorSlabStatsStep, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_graph_target(&graph_name)?;
    crate::vector_sync::forward_admin_vector_slab_stats_step(target, cursor, max_pages, index_id)
        .await
        .map_err(RouterError::Internal)
}

pub(crate) async fn admin_vector_centroid_cache_status(
    graph_name: String,
) -> Result<VectorCentroidCacheStatus, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_graph_target(&graph_name)?;
    crate::vector_sync::forward_admin_vector_centroid_cache_status(target)
        .await
        .map_err(RouterError::Internal)
}

pub(crate) async fn admin_vector_maintenance_status(
    graph_name: String,
    index_id: u32,
) -> Result<VectorMaintenanceState, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_maintenance_status(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

pub(crate) async fn admin_start_vector_rebuild(
    graph_name: String,
    index_id: u32,
    nlist: u32,
    sample_limit: u32,
) -> Result<(), RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_start_vector_rebuild(target, index_id, nlist, sample_limit)
        .await
        .map_err(RouterError::Internal)
}

pub(crate) async fn admin_start_vector_rebuild_if_recommended(
    graph_name: String,
    index_id: u32,
    attested_page_health: VectorPartitionPageHealth,
    policy: VectorMaintenancePolicy,
    target_nlist: Option<u32>,
    sample_limit: u32,
) -> Result<VectorMaintenanceRecommendation, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_start_vector_rebuild_if_recommended(
        target,
        index_id,
        attested_page_health,
        policy,
        target_nlist,
        sample_limit,
    )
    .await
    .map_err(RouterError::Internal)
}

pub(crate) async fn admin_vector_rebuild_step(
    graph_name: String,
    index_id: u32,
    max_subjects: u32,
) -> Result<VectorRebuildStatus, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_rebuild_step(target, index_id, max_subjects)
        .await
        .map_err(RouterError::Internal)
}

pub(crate) async fn admin_publish_vector_rebuild(
    graph_name: String,
    index_id: u32,
) -> Result<(), RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_publish_vector_rebuild(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

pub(crate) async fn admin_abort_vector_rebuild(
    graph_name: String,
    index_id: u32,
) -> Result<(), RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_abort_vector_rebuild(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

pub(crate) async fn admin_vector_rebuild_cleanup_step(
    graph_name: String,
    index_id: u32,
    max_work: u32,
) -> Result<VectorRebuildStatus, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_rebuild_cleanup_step(target, index_id, max_work)
        .await
        .map_err(RouterError::Internal)
}

pub(crate) async fn admin_vector_centroid_cache_warmup(
    graph_name: String,
    index_id: u32,
) -> Result<VectorCentroidCacheStatus, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_centroid_cache_warmup(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

pub(crate) async fn admin_vector_centroid_cache_clear(
    graph_name: String,
) -> Result<VectorCentroidCacheStatus, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_graph_target(&graph_name)?;
    crate::vector_sync::forward_admin_vector_centroid_cache_clear(target)
        .await
        .map_err(RouterError::Internal)
}

pub(crate) async fn admin_vector_maintenance_reset(
    graph_name: String,
    index_id: u32,
) -> Result<(), RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_maintenance_reset(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

// --- ADR 0031 Slice 10: Router-owned maintenance policy catalog + push step (commit 3) ---
//
// Policy CRUD is Router-local SSOT (no forwarding); the push step snapshots the policy and forwards
// one bounded unit to the vector canister. Policy authorship is `authorize_index_ddl` (the DDL admin
// family that owns index definitions); stepping/reset/reads are `authorize_vector_maintenance`.

/// Admin: create or replace the maintenance policy for one vector index. Validated against the
/// Router-owned definition (`recommended_*_bps <= required_*_bps`, nonzero budgets, def exists).
pub(crate) fn admin_set_vector_maintenance_policy(
    args: SetVectorMaintenancePolicyArgs,
) -> Result<(), RouterError> {
    use crate::facade::stable::vector_maintenance_policy::{
        VectorMaintenancePolicyRecord, set_policy,
    };
    crate::rbac::authorize_index_ddl(&msg_caller())?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&args.logical_graph_name)?;
    set_policy(VectorMaintenancePolicyRecord {
        graph_id,
        index_id: args.index_id,
        enabled: args.enabled,
        policy: args.policy,
        target_nlist: args.target_nlist,
        sample_limit: args.sample_limit,
        scan_max_pages: args.scan_max_pages,
        rebuild_max_subjects: args.rebuild_max_subjects,
        cleanup_max_work: args.cleanup_max_work,
    })
}

/// Admin: disable (but keep) the maintenance policy for one vector index. The push step becomes a
/// no-op until it is re-enabled. Distinct from `admin_vector_maintenance_reset`, which clears the
/// vector-canister execution state.
pub(crate) fn admin_disable_vector_maintenance_policy(
    graph_name: String,
    index_id: u32,
) -> Result<(), RouterError> {
    crate::rbac::authorize_index_ddl(&msg_caller())?;
    let graph_id = RouterStore::new().resolve_graph_id(&graph_name)?;
    crate::facade::stable::vector_maintenance_policy::disable_policy(graph_id, index_id)
}

/// Admin: delete the maintenance policy for one vector index.
pub(crate) fn admin_delete_vector_maintenance_policy(
    graph_name: String,
    index_id: u32,
) -> Result<bool, RouterError> {
    crate::rbac::authorize_index_ddl(&msg_caller())?;
    let graph_id = RouterStore::new().resolve_graph_id(&graph_name)?;
    Ok(crate::facade::stable::vector_maintenance_policy::delete_policy(graph_id, index_id))
}

/// Query: the maintenance policy for one vector index, if any.
pub(crate) fn vector_maintenance_policy(
    graph_name: String,
    index_id: u32,
) -> Result<Option<VectorMaintenancePolicyView>, RouterError> {
    let graph_id = RouterStore::new().resolve_graph_id(&graph_name)?;
    Ok(
        crate::facade::stable::vector_maintenance_policy::get_policy(graph_id, index_id)
            .map(VectorMaintenancePolicyView::from),
    )
}

/// Query: all maintenance policies in a graph.
pub(crate) fn list_vector_maintenance_policies(
    graph_name: String,
) -> Result<Vec<VectorMaintenancePolicyView>, RouterError> {
    let graph_id = RouterStore::new().resolve_graph_id(&graph_name)?;
    Ok(
        crate::facade::stable::vector_maintenance_policy::list_policies(graph_id)
            .into_iter()
            .map(VectorMaintenancePolicyView::from)
            .collect(),
    )
}

/// Admin push step (ADR 0031 Slice 10): resolve + RBAC + readiness, load the policy, and forward one
/// bounded maintenance unit to the vector canister. Returns `Disabled` (a no-op) when no policy
/// exists or it is disabled. One call = one bounded vector unit; publish stays explicit.
pub(crate) async fn admin_vector_maintenance_step(
    graph_name: String,
    index_id: u32,
) -> Result<VectorMaintenanceStepOutcome, RouterError> {
    use gleaph_graph_kernel::vector_index::VectorMaintenanceStepRequest;
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let graph_id = RouterStore::new().resolve_graph_id(&graph_name)?;
    let policy = crate::facade::stable::vector_maintenance_policy::get_policy(graph_id, index_id);
    let Some(policy) = policy.filter(|p| p.enabled) else {
        return Ok(VectorMaintenanceStepOutcome::Disabled);
    };
    // Readiness/target gate only after we know a policy is enabled, so a disabled index is a clean
    // no-op rather than a fail-closed error.
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    let req = VectorMaintenanceStepRequest {
        policy: policy.policy,
        target_nlist: policy.target_nlist,
        sample_limit: policy.sample_limit,
        scan_max_pages: policy.scan_max_pages,
        rebuild_max_subjects: policy.rebuild_max_subjects,
        cleanup_max_work: policy.cleanup_max_work,
    };
    crate::vector_sync::forward_admin_vector_maintenance_step(target, index_id, req)
        .await
        .map(VectorMaintenanceStepOutcome::Stepped)
        .map_err(RouterError::Internal)
}

/// Query (ADR 0031 Slice 10): Router-owned policy/readiness plus the forwarded vector-canister
/// maintenance + rebuild state when the target is reachable. Cursors are reported present/absent.
pub(crate) async fn vector_maintenance_status(
    graph_name: String,
    index_id: u32,
) -> Result<VectorMaintenanceStatusView, RouterError> {
    use crate::facade::stable::{vector_activation, vector_index_catalog};
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&graph_name)?;
    let def = vector_index_catalog::get_vector_index(graph_id, index_id)
        .ok_or_else(|| RouterError::NotFound(format!("vector index {index_id}")))?;
    let policy_enabled =
        crate::facade::stable::vector_maintenance_policy::get_policy(graph_id, index_id)
            .is_some_and(|p| p.enabled);
    let target = def.target.map(|t| t.canister);
    let global_enabled = vector_activation::vector_dispatch_globally_enabled();
    let dispatch_ready = store.graph_vector_dispatch_ready(graph_id);
    let block_reason = vector_index_catalog::activation_block_reason(
        def.activation_state,
        global_enabled,
        dispatch_ready,
    );
    let blocked_reason = block_reason.as_ref().map(|r| r.to_string());
    // Forward only when the gate is open and a non-anonymous target exists; otherwise report
    // Router-owned facts with `None` execution state.
    let (maintenance_state, rebuild_status) = match target {
        Some(canister) if block_reason.is_none() && canister != Principal::anonymous() => {
            let maintenance_state =
                crate::vector_sync::forward_admin_vector_maintenance_status(canister, index_id)
                    .await
                    .ok()
                    .map(crate::types::VectorMaintenanceStateView::from);
            let rebuild_status =
                crate::vector_sync::forward_admin_vector_rebuild_status(canister, index_id)
                    .await
                    .ok();
            (maintenance_state, rebuild_status)
        }
        _ => (None, None),
    };
    Ok(VectorMaintenanceStatusView {
        index_id,
        policy_enabled,
        target,
        dispatch_ready,
        blocked_reason,
        maintenance_state,
        rebuild_status,
    })
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

#[cfg(test)]
mod vertex_embedding_ingestion_tests {
    use super::*;
    use crate::facade::store::RouterStore;
    use crate::init::RouterInitArgs;
    use crate::types::{
        AdminRegisterShardArgs, GraphRegistryEntry, GraphStatus, ProvisioningState,
    };
    use candid::Principal;
    use gleaph_graph_kernel::entry::GraphId;
    use gleaph_graph_kernel::federation::{GlobalVertexId, ShardId, encode_global_vertex_id};
    use gleaph_graph_kernel::vector_index::{VectorEncoding, VectorIndexKind, VectorMetric};
    use std::collections::BTreeSet;

    fn admin() -> Principal {
        Principal::from_slice(&[1; 29])
    }
    fn graph_canister() -> Principal {
        Principal::self_authenticating([2; 32])
    }
    fn index_canister() -> Principal {
        Principal::self_authenticating([3; 32])
    }

    fn setup() -> (RouterStore, GraphId) {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            provision_canister: None,
        });
        let a = admin();
        crate::facade::auth::grant_admins(&[a]);
        store
            .admin_register_graph(
                a,
                GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    graph_name: "ingest.graph".to_owned(),
                    canister_id: Principal::management_canister(),
                    owner: a,
                    admins: BTreeSet::new(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home: false,
                },
            )
            .expect("register graph");
        let graph_id = crate::facade::stable::graph_catalog::lookup_graph_id("ingest.graph")
            .expect("graph id");
        futures::executor::block_on(store.admin_register_shard(
            a,
            AdminRegisterShardArgs {
                shard_id: ShardId::new(0),
                graph_canister: graph_canister(),
                index_canister: index_canister(),
                logical_graph_name: "ingest.graph".to_owned(),
            },
        ))
        .expect("register shard");
        let name_id = crate::facade::stable::embedding_name_catalog::intern_embedding_name(
            graph_id,
            "title_vec",
        )
        .expect("intern");
        crate::facade::stable::vector_index_catalog::register_vector_index(
            graph_id,
            1,
            name_id,
            VectorIndexKind::IvfFlat,
            VectorMetric::L2Squared,
            VectorEncoding::F32,
            4,
            None,
            false,
        )
        .expect("register vector index");
        (store, graph_id)
    }

    fn graph_id() -> GraphId {
        crate::facade::stable::graph_catalog::lookup_graph_id("ingest.graph").expect("graph id")
    }

    fn encoded_vertex(local: u32) -> Vec<u8> {
        let store = RouterStore::new();
        let key = store
            .graph_element_id_encoding_key(graph_id())
            .expect("key");
        let global = GlobalVertexId::new(ShardId::new(0), local);
        encode_global_vertex_id(&key, global).0.to_vec()
    }

    fn encoded_vertex_on_shard(local: u32, shard_id: u32) -> Vec<u8> {
        let store = RouterStore::new();
        let key = store
            .graph_element_id_encoding_key(graph_id())
            .expect("key");
        let global = GlobalVertexId::new(ShardId::new(shard_id), local);
        encode_global_vertex_id(&key, global).0.to_vec()
    }

    fn args(
        encoded: Vec<u8>,
        name: &str,
        values: Vec<f32>,
    ) -> crate::types::AdminIngestVertexEmbeddingArgs {
        crate::types::AdminIngestVertexEmbeddingArgs {
            logical_graph_name: "ingest.graph".to_string(),
            encoded_vertex_id: encoded,
            embedding_name: name.to_string(),
            values,
        }
    }

    #[test]
    fn resolve_valid_ingestion() {
        let (store, _graph_id) = setup();
        let a = args(encoded_vertex(0), "title_vec", vec![1.0, 2.0, 3.0, 4.0]);
        let (canister, ingestion) =
            resolve_vertex_embedding_ingestion(&a, &store).expect("resolve");
        assert_eq!(canister, graph_canister());
        assert_eq!(ingestion.local_vertex_id, 0);
        assert_eq!(ingestion.spec.dims, 4);
        assert_eq!(ingestion.values, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn empty_embedding_name_rejected() {
        let (store, _graph_id) = setup();
        let a = args(encoded_vertex(0), "", vec![1.0, 2.0, 3.0, 4.0]);
        let err = resolve_vertex_embedding_ingestion(&a, &store).expect_err("empty name");
        assert!(
            matches!(err, RouterError::InvalidArgument(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn malformed_encoded_id_rejected() {
        let (store, _graph_id) = setup();
        let a = crate::types::AdminIngestVertexEmbeddingArgs {
            logical_graph_name: "ingest.graph".to_string(),
            encoded_vertex_id: vec![1, 2, 3],
            embedding_name: "title_vec".to_string(),
            values: vec![1.0, 2.0, 3.0, 4.0],
        };
        let err = resolve_vertex_embedding_ingestion(&a, &store).expect_err("malformed id");
        assert!(
            matches!(err, RouterError::InvalidArgument(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn unregistered_shard_rejected() {
        let (store, _graph_id) = setup();
        let a = args(
            encoded_vertex_on_shard(0, 99),
            "title_vec",
            vec![1.0, 2.0, 3.0, 4.0],
        );
        let err = resolve_vertex_embedding_ingestion(&a, &store).expect_err("unregistered shard");
        assert!(
            matches!(err, RouterError::ShardNotRegistered),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn unknown_embedding_name_rejected() {
        let (store, _graph_id) = setup();
        let a = args(encoded_vertex(0), "unknown_vec", vec![1.0, 2.0, 3.0, 4.0]);
        let err = resolve_vertex_embedding_ingestion(&a, &store).expect_err("unknown embedding");
        assert!(
            matches!(err, RouterError::NotFound(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn dimension_mismatch_rejected() {
        let (store, _graph_id) = setup();
        let a = args(encoded_vertex(0), "title_vec", vec![1.0, 2.0]);
        let err = resolve_vertex_embedding_ingestion(&a, &store).expect_err("dimension mismatch");
        assert!(
            matches!(err, RouterError::InvalidArgument(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn non_finite_value_rejected() {
        let (store, _graph_id) = setup();
        let a = args(
            encoded_vertex(0),
            "title_vec",
            vec![1.0, 2.0, f32::NAN, 4.0],
        );
        let err = resolve_vertex_embedding_ingestion(&a, &store).expect_err("non-finite");
        assert!(
            matches!(err, RouterError::InvalidArgument(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn unauthorized_caller_rejected_by_rbac() {
        assert!(
            matches!(
                crate::rbac::authorize_index_ddl(&Principal::anonymous()),
                Err(RouterError::Forbidden)
            ),
            "anonymous caller must not pass index DDL authorization"
        );
    }
}

#[cfg(test)]
mod provision_config_upgrade_tests {
    use super::*;
    use crate::facade::stable::provision_config::{
        load_provision_runtime_config, save_provision_runtime_config,
    };
    use crate::init::validate_provision_principal;
    use crate::provisioning::config::ProvisionRuntimeConfig;
    use candid::Principal;

    fn canonical_principal() -> Principal {
        Principal::self_authenticating([1; 32])
    }

    #[test]
    fn test_validate_provision_principal_accepts_none_and_non_anonymous() {
        assert!(validate_provision_principal(&None).is_ok());
        assert!(
            validate_provision_principal(&Some(Principal::self_authenticating([2; 32]))).is_ok()
        );
        assert_eq!(
            validate_provision_principal(&Some(Principal::anonymous())),
            Err("provision_canister cannot be anonymous")
        );
    }

    #[test]
    fn test_post_upgrade_anonymous_override_rejected_preserves_canonical() {
        // Seed a canonical durable binding.
        let canonical = canonical_principal();
        let canonical_config = ProvisionRuntimeConfig {
            provision_canister: Some(canonical),
        };
        save_provision_runtime_config(&canonical_config);
        let durable = load_provision_runtime_config();

        // An anonymous override must be rejected: the resolver returns Err, the durable
        // record is preserved, and post_upgrade would trap.
        let result = resolve_provision_canister_for_upgrade(Some(Principal::anonymous()), &durable);
        assert_eq!(result, Err("provision_canister cannot be anonymous"));
        assert_eq!(
            load_provision_runtime_config(),
            durable,
            "durable record must not be overwritten by an invalid override"
        );
    }

    #[test]
    fn test_post_upgrade_valid_override_updates_canonical() {
        let canonical = canonical_principal();
        let replacement = Principal::self_authenticating([7; 32]);
        save_provision_runtime_config(&ProvisionRuntimeConfig {
            provision_canister: Some(canonical),
        });

        let durable = load_provision_runtime_config();
        let result = resolve_provision_canister_for_upgrade(Some(replacement), &durable).unwrap();
        assert_eq!(result, Some(replacement));
        assert_eq!(
            load_provision_runtime_config(),
            ProvisionRuntimeConfig {
                provision_canister: Some(replacement),
            }
        );
    }

    #[test]
    fn test_post_upgrade_none_override_uses_durable() {
        let canonical = canonical_principal();
        save_provision_runtime_config(&ProvisionRuntimeConfig {
            provision_canister: Some(canonical),
        });

        let result =
            resolve_provision_canister_for_upgrade(None, &load_provision_runtime_config()).unwrap();
        assert_eq!(result, Some(canonical));
    }

    mod upgrade_arg_decode_tests {
        use super::*;
        use crate::init::RouterInitArgs;
        use candid::Encode;

        #[test]
        fn valid_upgrade_args_decodes() {
            let principal = Principal::self_authenticating([1; 32]);
            let bytes = Encode!(&RouterUpgradeArgs {
                provision_canister: Some(principal),
            })
            .expect("encode");
            let decoded = decode_upgrade_args(&bytes).expect("decoded");
            assert_eq!(decoded.provision_canister, Some(principal));
        }

        #[test]
        fn absent_provision_decodes_to_none_override() {
            let bytes = Encode!(&RouterUpgradeArgs {
                provision_canister: None,
            })
            .expect("encode");
            let decoded = decode_upgrade_args(&bytes).expect("decoded");
            assert_eq!(decoded.provision_canister, None);
        }

        #[test]
        fn router_init_args_decode_ignores_init_only_fields() {
            // Candid record subtyping lets a RouterInitArgs payload decode as
            // RouterUpgradeArgs: extra fields (issuing_principal, initial_admins)
            // are ignored. Only the provision_canister override matters.
            let admin = Principal::self_authenticating([2; 32]);
            let provision = Principal::self_authenticating([3; 32]);
            let bytes = Encode!(&RouterInitArgs {
                issuing_principal: admin,
                initial_admins: vec![],
                provision_canister: Some(provision),
            })
            .expect("encode");
            let decoded = decode_upgrade_args(&bytes).expect("decoded");
            assert_eq!(decoded.provision_canister, Some(provision));
        }
    }
}
