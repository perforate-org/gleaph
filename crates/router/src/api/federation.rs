//! L3 federation surface (ADR 0056 §5).
//!
//! Client-invisible: the graph/index/vector canisters and operator deep tooling. Shard resolution,
//! catalogs, `router_ack`, vector wiring, physical diagnostics, manual rebuild control, and
//! maintenance policy CRUD. Useful-but-unused operator functionality is retained here rather than
//! deleted. `provision_graph` is a documented-internal seam (Slice A) kept only for `adr0035`
//! outbound/ack coverage; it is removed in Slice B.

use candid::Principal;
use ic_cdk::api::msg_caller;
use ic_cdk_macros::{query, update};

use crate::facade::auth;
use crate::facade::store::RouterStore;
use crate::state::RouterError;
use crate::types;

#[query]
fn get_shard(
    logical_graph_name: String,
    shard_id: types::ShardId,
) -> Result<types::ShardRegistryEntry, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().resolve_shard(graph_id, shard_id)
}

#[query]
fn get_graph_id(graph_name: String) -> Result<gleaph_graph_kernel::entry::GraphId, RouterError> {
    RouterStore::new().resolve_graph_id_authorized(&graph_name, msg_caller())
}

#[query]
fn get_id_encoding_key(logical_graph_name: String) -> Result<[u8; 16], RouterError> {
    auth::require_admin(&msg_caller())?;
    let graph_id = RouterStore::new().resolve_graph_id(&logical_graph_name)?;
    Ok(RouterStore::new()
        .graph_element_id_encoding_key(graph_id)?
        .0)
}

#[query]
fn list_shards(logical_graph_name: String) -> Result<Vec<types::ShardRegistryEntry>, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().list_shards_for_graph_id(graph_id)
}

/// Router-sourced snapshot of which properties are indexed for a graph (ADR 0023
/// D1/D3/P2). Graph shards consult this ephemerally per operation — including the
/// async maintenance tick that re-keys postings after compaction — so they never
/// persist derived index state across the upgrade boundary.
#[query]
fn get_indexed_property_catalog(
    logical_graph_name: String,
) -> Result<gleaph_graph_kernel::index::IndexedPropertyCatalog, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    Ok(crate::facade::stable::indexed_catalog::load_indexed_property_catalog(graph_id))
}

#[query]
fn lookup_vertex_label_id(
    logical_graph_name: String,
    name: String,
) -> Result<types::VertexLabelId, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().lookup_vertex_label_id(graph_id, &name)
}

#[query]
fn lookup_edge_label_id(
    logical_graph_name: String,
    name: String,
) -> Result<types::EdgeLabelId, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().lookup_edge_label_id(graph_id, &name)
}

#[query]
fn lookup_property_id(
    logical_graph_name: String,
    name: String,
) -> Result<types::PropertyId, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().lookup_property_id(graph_id, &name)
}

#[query]
fn reverse_vertex_label_name(
    logical_graph_name: String,
    label_id: types::VertexLabelId,
) -> Result<String, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().reverse_vertex_label_name(graph_id, label_id)
}

#[query]
fn reverse_edge_label_name(
    logical_graph_name: String,
    label_id: types::EdgeLabelId,
) -> Result<String, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().reverse_edge_label_name(graph_id, label_id)
}

#[query]
fn reverse_property_name(
    logical_graph_name: String,
    property_id: types::PropertyId,
) -> Result<String, RouterError> {
    let graph_id =
        RouterStore::new().resolve_graph_id_authorized(&logical_graph_name, msg_caller())?;
    RouterStore::new().reverse_property_name(graph_id, property_id)
}

#[update]
fn update_graph_status(
    graph_name: String,
    status: gleaph_gql_ic::graph_registry::GraphStatus,
    version: u64,
) -> Result<(), RouterError> {
    RouterStore::new().admin_update_graph_status(msg_caller(), &graph_name, status, version)
}

#[update]
async fn register_shard(args: types::AdminRegisterShardArgs) -> Result<(), RouterError> {
    RouterStore::new()
        .admin_register_shard(msg_caller(), args)
        .await
}

#[update]
async fn unregister_shard(
    logical_graph_name: String,
    shard_id: types::ShardId,
) -> Result<(), RouterError> {
    RouterStore::new()
        .admin_unregister_shard(msg_caller(), &logical_graph_name, shard_id)
        .await
}

/// Verify router registry denormalization invariants (regions 1–5, 15–16). `Ok(())`
/// means consistent; `Err(Internal(detail))` reports the first divergence. Read-only
/// oracle so registry consistency can be checked on demand, including across upgrades.
#[query]
fn check_registry_invariants() -> Result<(), RouterError> {
    auth::require_admin(&msg_caller())?;
    RouterStore::new()
        .check_registry_invariants()
        .map_err(RouterError::Internal)
}

/// Evict expired client-mutation idempotency records in a bounded, paginated pass.
/// Call repeatedly, feeding `next_cursor` back as `start_after`, until `done`.
#[update]
fn sweep_expired_mutation_keys(
    args: types::AdminSweepMutationKeysStepArgs,
) -> Result<types::AdminSweepMutationKeysStepResult, RouterError> {
    RouterStore::new().admin_sweep_expired_client_mutation_keys(
        msg_caller(),
        args.start_after,
        args.max_scan,
    )
}

/// Operator recovery: clear a stuck `in_progress` claim on a shard's backfill
/// cursor (`Role::Admin`). Use only when no step is in flight for the shard.
#[update]
fn reset_backfill_claim(args: types::AdminResetBackfillClaimArgs) -> Result<(), RouterError> {
    RouterStore::new().admin_reset_backfill_claim(msg_caller(), &args)
}

// --- Vector wiring (ADR 0031 Slice 3/4) ---

/// Assign the single dispatch target of a vector index (ADR 0031 Slice 3; `authorize_index_ddl`).
/// Assignment is immutable after the first successful set; Slice 3 stores the target as
/// inspect-only metadata.
#[update]
fn set_vector_index_target(args: types::SetVectorIndexTargetArgs) -> Result<(), RouterError> {
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

/// Resolve a vector index's single dispatch target principal (ADR 0031 Slice 3, inspect-only).
#[query]
fn get_vector_index_target(
    logical_graph_name: String,
    index_id: u32,
) -> Result<Principal, RouterError> {
    use crate::facade::stable::vector_index_catalog;

    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&logical_graph_name)?;
    vector_index_catalog::vector_index_target_for(graph_id, index_id)
}

/// Report a vector index's activation state and, while fail-closed, the blocking reason
/// (ADR 0031 Slice 3).
#[query]
fn get_vector_index_status(
    logical_graph_name: String,
    index_id: u32,
) -> Result<types::VectorIndexActivationStatus, RouterError> {
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
    Ok(types::VectorIndexActivationStatus {
        index_id,
        activation_state: vector_index_catalog::activation_state_view(
            vector_index_catalog::effective_activation_state(def.activation_state, dispatch_ready),
        ),
        blocked_reason,
    })
}

/// Flip the global vector-dispatch activation flag (ADR 0031 Slice 4; Admin only). `false` keeps
/// production dispatch/backfill fail-closed across all graphs. Reversible.
#[update]
fn set_vector_dispatch_enabled(enabled: bool) -> Result<(), RouterError> {
    crate::rbac::authorize_vector_activation(&msg_caller())?;
    crate::facade::stable::vector_activation::set_vector_dispatch_globally_enabled(enabled);
    Ok(())
}

/// Read the global vector-dispatch activation flag (ADR 0031 Slice 4).
#[query]
fn get_vector_dispatch_enabled() -> bool {
    crate::facade::stable::vector_activation::vector_dispatch_globally_enabled()
}

/// Wire (or retrofit) a derived vector-index target onto an already-registered shard and drive the
/// attach handshake (ADR 0031 Slice 4; Admin only). Idempotent; one vector-index target per graph.
#[update]
async fn attach_vector_shard(
    args: types::AdminAttachVectorIndexShardArgs,
) -> Result<(), RouterError> {
    let caller = msg_caller();
    crate::rbac::authorize_vector_activation(&caller)?;
    RouterStore::new()
        .admin_attach_vector_index_shard(caller, args)
        .await
}

// --- Vector physical diagnostics (ADR 0031 Slice 10, forwarded) ---
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

/// Head-only O(`nlist`) partition-health summary, forwarded to the activated vector target.
#[query(composite = true)]
async fn get_vector_partition_health(
    graph_name: String,
    index_id: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorPartitionHealthSummary, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_partition_health(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

/// Bounded page-meta tombstone-health scan step, forwarded to the activated vector target.
#[query(composite = true)]
async fn scan_partition_health(
    graph_name: String,
    index_id: u32,
    cursor: Option<Vec<u8>>,
    max_pages: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorPartitionHealthStep, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_partition_health_step(
        target, index_id, cursor, max_pages,
    )
    .await
    .map_err(RouterError::Internal)
}

/// O(1) rebuild status, forwarded to the activated vector target.
#[query(composite = true)]
async fn get_vector_rebuild_status(
    graph_name: String,
    index_id: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorRebuildStatus, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_rebuild_status(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

/// Derived slab-space observability, forwarded to the graph's vector target (`index_id` scopes the
/// logical counters; the slab physical facts are whole-slab global).
#[query(composite = true)]
async fn get_vector_slab_stats(
    graph_name: String,
    index_id: Option<u32>,
) -> Result<gleaph_graph_kernel::vector_index::VectorSlabStats, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_graph_target(&graph_name)?;
    crate::vector_sync::forward_admin_vector_slab_stats(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

/// Cursor/budgeted slab-stats scan step, forwarded to the graph's vector target.
#[query(composite = true)]
async fn scan_slab_stats(
    graph_name: String,
    cursor: Option<Vec<u8>>,
    max_pages: u32,
    index_id: Option<u32>,
) -> Result<gleaph_graph_kernel::vector_index::VectorSlabStatsStep, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_graph_target(&graph_name)?;
    crate::vector_sync::forward_admin_vector_slab_stats_step(target, cursor, max_pages, index_id)
        .await
        .map_err(RouterError::Internal)
}

/// Heap centroid cache status, forwarded to the graph's vector target.
#[query(composite = true)]
async fn get_vector_centroid_cache(
    graph_name: String,
) -> Result<gleaph_graph_kernel::vector_index::VectorCentroidCacheStatus, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_graph_target(&graph_name)?;
    crate::vector_sync::forward_admin_vector_centroid_cache_status(target)
        .await
        .map_err(RouterError::Internal)
}

/// Warm the heap centroid cache on the activated vector target.
#[update]
async fn warm_vector_centroid_cache(
    graph_name: String,
    index_id: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorCentroidCacheStatus, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_centroid_cache_warmup(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

/// Clear the entire heap centroid cache on the graph's vector target.
#[update]
async fn clear_vector_centroid_cache(
    graph_name: String,
) -> Result<gleaph_graph_kernel::vector_index::VectorCentroidCacheStatus, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_graph_target(&graph_name)?;
    crate::vector_sync::forward_admin_vector_centroid_cache_clear(target)
        .await
        .map_err(RouterError::Internal)
}

// --- Manual rebuild control (ADR 0031 Slice 10) ---

/// Begin a shadow-version rebuild on the activated vector target.
#[update]
async fn start_vector_rebuild(
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

/// Drive one bounded rebuild step on the activated vector target.
#[update]
async fn advance_vector_rebuild(
    graph_name: String,
    index_id: u32,
    max_subjects: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorRebuildStatus, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_rebuild_step(target, index_id, max_subjects)
        .await
        .map_err(RouterError::Internal)
}

/// Publish a `ReadyToPublish` rebuild on the activated vector target.
#[update]
async fn publish_vector_rebuild(graph_name: String, index_id: u32) -> Result<(), RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_publish_vector_rebuild(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

/// Abort an in-flight rebuild on the activated vector target.
#[update]
async fn abort_vector_rebuild(graph_name: String, index_id: u32) -> Result<(), RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_abort_vector_rebuild(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

/// Drive one bounded cleanup/abort teardown step on the activated vector target.
#[update]
async fn advance_vector_rebuild_cleanup(
    graph_name: String,
    index_id: u32,
    max_work: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorRebuildStatus, RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_rebuild_cleanup_step(target, index_id, max_work)
        .await
        .map_err(RouterError::Internal)
}

// --- Maintenance deep (ADR 0031 Slice 10) ---
//
// Policy CRUD is Router-local SSOT (no forwarding); the push step snapshots the policy and forwards
// one bounded unit to the vector canister. Policy authorship is `authorize_index_ddl` (the DDL admin
// family that owns index definitions); stepping/reset/reads are `authorize_vector_maintenance`.

/// Reset the maintenance execution state to `Idle` (incl. `Failed`) on the activated vector target.
/// Does not abort an in-flight rebuild (use `abort_vector_rebuild`) or change Router policy.
#[update]
async fn reset_vector_maintenance(graph_name: String, index_id: u32) -> Result<(), RouterError> {
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let target = resolve_vector_maintenance_target(&graph_name, index_id)?;
    crate::vector_sync::forward_admin_vector_maintenance_reset(target, index_id)
        .await
        .map_err(RouterError::Internal)
}

/// Create or replace the Router-owned maintenance policy for one vector index (DDL admin).
#[update]
fn set_vector_maintenance_policy(
    args: types::SetVectorMaintenancePolicyArgs,
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

/// Disable (but keep) the maintenance policy for one vector index (DDL admin).
#[update]
fn disable_vector_maintenance_policy(graph_name: String, index_id: u32) -> Result<(), RouterError> {
    crate::rbac::authorize_index_ddl(&msg_caller())?;
    let graph_id = RouterStore::new().resolve_graph_id(&graph_name)?;
    crate::facade::stable::vector_maintenance_policy::disable_policy(graph_id, index_id)
}

/// Delete the maintenance policy for one vector index (DDL admin). Returns whether one existed.
#[update]
fn delete_vector_maintenance_policy(
    graph_name: String,
    index_id: u32,
) -> Result<bool, RouterError> {
    crate::rbac::authorize_index_ddl(&msg_caller())?;
    let graph_id = RouterStore::new().resolve_graph_id(&graph_name)?;
    Ok(crate::facade::stable::vector_maintenance_policy::delete_policy(graph_id, index_id))
}

/// The maintenance policy for one vector index, if any.
#[query]
fn get_vector_maintenance_policy(
    graph_name: String,
    index_id: u32,
) -> Result<Option<types::VectorMaintenancePolicyView>, RouterError> {
    let graph_id = RouterStore::new().resolve_graph_id(&graph_name)?;
    Ok(
        crate::facade::stable::vector_maintenance_policy::get_policy(graph_id, index_id)
            .map(types::VectorMaintenancePolicyView::from),
    )
}

/// All maintenance policies in a graph.
#[query]
fn list_vector_maintenance_policies(
    graph_name: String,
) -> Result<Vec<types::VectorMaintenancePolicyView>, RouterError> {
    let graph_id = RouterStore::new().resolve_graph_id(&graph_name)?;
    Ok(
        crate::facade::stable::vector_maintenance_policy::list_policies(graph_id)
            .into_iter()
            .map(types::VectorMaintenancePolicyView::from)
            .collect(),
    )
}

/// Advance one bounded maintenance unit for an enabled policy; `Disabled` no-op otherwise.
#[update]
async fn advance_vector_maintenance(
    graph_name: String,
    index_id: u32,
) -> Result<types::VectorMaintenanceStepOutcome, RouterError> {
    use gleaph_graph_kernel::vector_index::VectorMaintenanceStepRequest;
    crate::rbac::authorize_vector_maintenance(&msg_caller())?;
    let graph_id = RouterStore::new().resolve_graph_id(&graph_name)?;
    let policy = crate::facade::stable::vector_maintenance_policy::get_policy(graph_id, index_id);
    let Some(policy) = policy.filter(|p| p.enabled) else {
        return Ok(types::VectorMaintenanceStepOutcome::Disabled);
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
        .map(types::VectorMaintenanceStepOutcome::Stepped)
        .map_err(RouterError::Internal)
}

/// Router policy/readiness plus forwarded vector-canister maintenance + rebuild state.
#[query(composite = true)]
async fn get_vector_maintenance_status(
    graph_name: String,
    index_id: u32,
) -> Result<types::VectorMaintenanceStatusView, RouterError> {
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
                    .map(types::VectorMaintenanceStateView::from);
            let rebuild_status =
                crate::vector_sync::forward_admin_vector_rebuild_status(canister, index_id)
                    .await
                    .ok();
            (maintenance_state, rebuild_status)
        }
        _ => (None, None),
    };
    Ok(types::VectorMaintenanceStatusView {
        index_id,
        policy_enabled,
        target,
        dispatch_ready,
        blocked_reason,
        maintenance_state,
        rebuild_status,
    })
}

/// **Internal seam (ADR 0056 §6 Slice A): not a client API.** Delegates to the shared provisioned
/// graph-admission flow in `provisioning::graph`. `register_graph` is the public graph-creation
/// surface (its provisioned fold calls the same flow); this endpoint is retained only so the
/// `adr0035` outbound/ack E2E coverage keeps running. Admin-only.
#[update]
async fn provision_graph(
    args: types::ProvisionGraphArgs,
) -> Result<types::ProvisionGraphResponse, RouterError> {
    let caller = ic_cdk::api::msg_caller();
    auth::require_admin(&caller)?;
    crate::provisioning::graph::provision_graph_flow(caller, args).await
}

/// Internal callback: the configured Provision canister acknowledges a completed
/// provisioning job and asks the Router to commit the terminal catalog state.
#[update]
fn router_ack(
    ack: gleaph_graph_kernel::provisioning::wire::RouterProvisionAck,
) -> Result<gleaph_graph_kernel::provisioning::wire::RouterAckResponse, RouterError> {
    use crate::provisioning::ack_handler::handle_router_ack;
    handle_router_ack(ic_cdk::api::msg_caller(), ack)
}
