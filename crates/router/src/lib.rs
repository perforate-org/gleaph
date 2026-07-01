//! Gleaph router canister — federation control plane (graph registry, shard registry).

#[cfg(feature = "canbench")]
mod bench;

#[cfg(feature = "pocket-ic-e2e")]
mod test_fault;

mod bulk_ingest_finalize;
mod canister;
mod constraint_ddl;
mod constraint_drop;
mod edge_backfill;
mod edge_index_direction;
mod edge_payload_ddl;
mod effect_recovery;
mod execution_path;
pub mod facade;
mod federation;
mod gql;
mod gql_search;
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
mod reclaim;
mod recovery;
mod seed;
pub mod state;
pub mod types;
mod use_graph;
mod use_graph_wire;
mod vector_sync;
mod vertex_property_backfill;

pub use facade::store::RouterStore;
pub use init::RouterInitArgs;
pub use state::RouterError;

use candid::Principal;
use ic_cdk_macros::{init, post_upgrade, query, update};

#[init]
fn init(args: RouterInitArgs) {
    canister::init(args);
    // ADR 0029 Phase 4: arm the autonomous saga recovery driver (no-op until there is work).
    recovery::arm_if_needed();
}

#[post_upgrade]
fn post_upgrade() {
    // Timers do not survive an upgrade; re-arm the recovery driver so non-terminal sagas
    // persisted across the upgrade still converge (ADR 0029 Phase 4).
    recovery::arm_if_needed();
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
fn resolve_shard(
    logical_graph_name: String,
    shard_id: types::ShardId,
) -> Result<types::ShardRegistryEntry, RouterError> {
    canister::resolve_shard(logical_graph_name, shard_id)
}

#[query]
fn lookup_graph_id(graph_name: String) -> Result<gleaph_graph_kernel::entry::GraphId, RouterError> {
    canister::lookup_graph_id(graph_name)
}

#[query]
fn graph_element_id_encoding_key(logical_graph_name: String) -> Result<[u8; 16], RouterError> {
    canister::graph_element_id_encoding_key(logical_graph_name)
}

#[query]
fn list_shards_for_graph(
    logical_graph_name: String,
) -> Result<Vec<types::ShardRegistryEntry>, RouterError> {
    canister::list_shards_for_graph(logical_graph_name)
}

#[query]
fn indexed_property_catalog(
    logical_graph_name: String,
) -> Result<gleaph_graph_kernel::index::IndexedPropertyCatalog, RouterError> {
    canister::indexed_property_catalog(logical_graph_name)
}

#[query]
fn lookup_vertex_label_id(
    logical_graph_name: String,
    name: String,
) -> Result<types::VertexLabelId, RouterError> {
    canister::lookup_vertex_label_id(logical_graph_name, name)
}

#[query]
fn lookup_edge_label_id(
    logical_graph_name: String,
    name: String,
) -> Result<types::EdgeLabelId, RouterError> {
    canister::lookup_edge_label_id(logical_graph_name, name)
}

#[query]
fn lookup_property_id(
    logical_graph_name: String,
    name: String,
) -> Result<types::PropertyId, RouterError> {
    canister::lookup_property_id(logical_graph_name, name)
}

#[query]
fn reverse_vertex_label_name(
    logical_graph_name: String,
    label_id: types::VertexLabelId,
) -> Result<String, RouterError> {
    canister::reverse_vertex_label_name(logical_graph_name, label_id)
}

#[query]
fn reverse_edge_label_name(
    logical_graph_name: String,
    label_id: types::EdgeLabelId,
) -> Result<String, RouterError> {
    canister::reverse_edge_label_name(logical_graph_name, label_id)
}

#[query]
fn reverse_property_name(
    logical_graph_name: String,
    property_id: types::PropertyId,
) -> Result<String, RouterError> {
    canister::reverse_property_name(logical_graph_name, property_id)
}

#[update]
async fn admin_register_graph(
    entry: gleaph_gql_ic::graph_registry::GraphRegistryEntry,
) -> Result<(), RouterError> {
    canister::admin_register_graph(entry).await
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
fn admin_unregister_graph(logical_graph_name: String) -> Result<(), RouterError> {
    canister::admin_unregister_graph(logical_graph_name)
}

#[update]
async fn admin_register_shard(args: types::AdminRegisterShardArgs) -> Result<(), RouterError> {
    canister::admin_register_shard(args).await
}

#[update]
async fn admin_unregister_shard(
    logical_graph_name: String,
    shard_id: types::ShardId,
) -> Result<(), RouterError> {
    canister::admin_unregister_shard(logical_graph_name, shard_id).await
}

/// Read-only oracle: verify router registry denormalization invariants (`Role::Admin`).
#[query]
fn admin_check_registry_invariants() -> Result<(), RouterError> {
    canister::admin_check_registry_invariants()
}

/// Evict expired client-mutation idempotency records (`Role::Admin`; call in a loop).
#[update]
fn admin_sweep_expired_client_mutation_keys(
    args: types::AdminSweepMutationKeysStepArgs,
) -> Result<types::AdminSweepMutationKeysStepResult, RouterError> {
    canister::admin_sweep_expired_client_mutation_keys(args)
}

#[update]
fn admin_intern_vertex_label(
    logical_graph_name: String,
    name: String,
) -> Result<types::VertexLabelId, RouterError> {
    canister::admin_intern_vertex_label(logical_graph_name, name)
}

#[update]
fn admin_intern_edge_label(
    logical_graph_name: String,
    name: String,
) -> Result<types::EdgeLabelId, RouterError> {
    canister::admin_intern_edge_label(logical_graph_name, name)
}

#[update]
fn admin_intern_property(
    logical_graph_name: String,
    name: String,
) -> Result<types::PropertyId, RouterError> {
    canister::admin_intern_property(logical_graph_name, name)
}

/// Read-only GQL: composite query (calls index + graph query endpoints).
#[query(composite = true)]
async fn gql_query(
    query: String,
    params: Vec<u8>,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
    gql::gql_query(query, params).await
}

/// Read-only GQL with an explicit ADR 0029 §5 read-consistency contract (Phase 3).
///
/// `Eventual` matches [`gql_query`]; `AtLeast(token)` enforces a retryable read-your-writes
/// barrier against the token's per-shard watermarks; `Canonical` is deferred and rejected.
#[query(composite = true)]
async fn gql_query_with_consistency(
    query: String,
    params: Vec<u8>,
    read_mode: gleaph_graph_kernel::plan_exec::ReadMode,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
    gql::gql_query_with_consistency(query, params, read_mode).await
}

/// Update-path GQL entrypoint for non-DML escape hatches; DML requires `gql_execute_idempotent`.
#[update]
async fn gql_execute(query: String, params: Vec<u8>) -> Result<u64, RouterError> {
    gql::gql_execute(query, params).await
}

/// Idempotent GQL update. Reuse `client_mutation_key` only for retries of the same mutation.
///
/// Returns the richer [`GqlQueryResult`](gleaph_graph_kernel::plan_exec::GqlQueryResult) so
/// clients can read the ADR 0029 federated mutation lifecycle `phase`, distinguishing a
/// durable canonical commit from full cross-canister projection convergence.
#[update]
async fn gql_execute_idempotent(
    query: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
    gql::gql_execute_idempotent(query, params, client_mutation_key).await
}

/// ADR 0029 Phase 4: pull-based status of a federated mutation for the calling principal.
#[query]
fn mutation_status(
    logical_graph_name: String,
    client_mutation_key: String,
) -> Result<types::MutationStatus, RouterError> {
    canister::mutation_status(logical_graph_name, client_mutation_key)
}

/// Test-only (`pocket-ic-e2e`): inject a projection-lagging federated saga referencing an
/// already-committed `mutation_id`, then arm the recovery timer. Lets the E2E suite drive the
/// autonomous recovery driver from `ProjectionPending` to `Completed` without a client retry.
#[cfg(feature = "pocket-ic-e2e")]
#[update]
fn test_inject_projection_pending_saga(
    logical_graph_name: String,
    client_mutation_key: String,
    mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    row_count: u64,
) -> Result<(), RouterError> {
    canister::test_inject_projection_pending_saga(
        logical_graph_name,
        client_mutation_key,
        mutation_id,
        row_count,
    )
}

/// Test-only (`pocket-ic-e2e`): declare a uniqueness constraint (admin-authorized, declare-on-empty)
/// so the E2E suite can exercise the ADR 0030 write-path lifecycle. Public `CREATE`/`DROP CONSTRAINT`
/// DDL remains `NotImplemented` (CREATE pending the publication decision, DROP pending a dedicated
/// lifecycle slice — ADR 0030 Revisions #14–#15).
#[cfg(feature = "pocket-ic-e2e")]
#[update]
fn test_declare_unique_constraint(
    logical_graph_name: String,
    constraint_name: String,
    label: String,
    property: String,
) -> Result<(), RouterError> {
    canister::test_declare_unique_constraint(logical_graph_name, constraint_name, label, property)
}

/// Test-only (`pocket-ic-e2e`): arm (or clear, with `0`) an ADR 0030 write-path fault injection so
/// the failure-injection e2e suite can reproduce trap boundaries (Try-then-trap, Confirm-then-trap).
/// Admin-authorized. See [`crate::test_fault`].
#[cfg(feature = "pocket-ic-e2e")]
#[update]
fn test_arm_fault(code: u8) -> Result<(), RouterError> {
    canister::test_arm_fault(code)
}

/// Test-only (`pocket-ic-e2e`): force a `Reserved` reservation into `Reclaiming` (admin), so the
/// failure-injection suite can prove a same-`ClaimId` retry is fenced during a reclaim proof.
#[cfg(feature = "pocket-ic-e2e")]
#[update]
fn test_force_reclaiming(
    logical_graph_name: String,
    label: String,
    property: String,
    value: String,
) -> Result<bool, RouterError> {
    canister::test_force_reclaiming(logical_graph_name, label, property, value)
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

/// Prepared read with an explicit ADR 0029 §5 read-consistency contract (Phase 3).
#[query(composite = true)]
async fn prepared_execute_query_with_consistency(
    name: String,
    params: Vec<u8>,
    read_mode: gleaph_graph_kernel::plan_exec::ReadMode,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
    prepared::prepared_execute_query_with_consistency(name, params, read_mode).await
}

#[update]
async fn prepared_execute_update(name: String, params: Vec<u8>) -> Result<u64, RouterError> {
    prepared::prepared_execute_update(name, params).await
}

/// Idempotent prepared update. Returns the richer
/// [`GqlQueryResult`](gleaph_graph_kernel::plan_exec::GqlQueryResult) carrying the ADR 0029
/// federated mutation lifecycle `phase`.
#[update]
async fn prepared_execute_update_idempotent(
    name: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
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

/// Register a derived vector index (ADR 0031 Slice 3; `authorize_index_ddl`). Returns whether the
/// definition was newly created. Production dispatch stays fail-closed until incarnation fencing.
#[update]
fn admin_register_vector_index(args: types::RegisterVectorIndexArgs) -> Result<bool, RouterError> {
    canister::admin_register_vector_index(args)
}

/// Set (or replace) the single dispatch target of a vector index (ADR 0031 Slice 3;
/// `authorize_index_ddl`). Slice 3 stores the target as inspect-only metadata.
#[update]
fn admin_set_vector_index_target(args: types::SetVectorIndexTargetArgs) -> Result<(), RouterError> {
    canister::admin_set_vector_index_target(args)
}

/// List the derived vector-index definitions registered for a logical graph (ADR 0031 Slice 3).
#[query]
fn list_vector_indexes(
    logical_graph_name: String,
) -> Result<Vec<types::VectorIndexInfo>, RouterError> {
    canister::list_vector_indexes(logical_graph_name)
}

/// Resolve a vector index's single dispatch target principal (ADR 0031 Slice 3, inspect-only).
#[query]
fn resolve_vector_index_target(
    logical_graph_name: String,
    index_id: u32,
) -> Result<Principal, RouterError> {
    canister::resolve_vector_index_target(logical_graph_name, index_id)
}

/// Report a vector index's activation state and, while fail-closed, the blocking reason
/// (ADR 0031 Slice 3).
#[query]
fn vector_index_activation_status(
    logical_graph_name: String,
    index_id: u32,
) -> Result<types::VectorIndexActivationStatus, RouterError> {
    canister::vector_index_activation_status(logical_graph_name, index_id)
}

/// Request a derived vector-index backfill step (ADR 0031; `authorize_index_ddl`). Fails closed with
/// `VectorDispatchActivationBlocked` until the global flag is on and the graph's shards are
/// vector-attached; the bounded production driver itself lands in Slice 5.
#[update]
async fn admin_vector_index_backfill_step(
    args: types::AdminVectorIndexBackfillStepArgs,
) -> Result<types::AdminVectorIndexBackfillStepResult, RouterError> {
    canister::admin_vector_index_backfill_step(args).await
}

/// Flip the global vector-dispatch activation flag (ADR 0031 Slice 4; Admin only). `false` keeps
/// production dispatch/backfill fail-closed across all graphs. Reversible.
#[update]
fn admin_set_vector_dispatch_activation(enabled: bool) -> Result<(), RouterError> {
    canister::admin_set_vector_dispatch_activation(enabled)
}

/// Read the global vector-dispatch activation flag (ADR 0031 Slice 4).
#[query]
fn vector_dispatch_activation_enabled() -> bool {
    canister::vector_dispatch_activation_enabled()
}

/// Wire (or retrofit) a derived vector-index target onto an already-registered shard and drive the
/// attach handshake (ADR 0031 Slice 4; Admin only). Idempotent; one vector-index target per graph.
#[update]
async fn admin_attach_vector_index_shard(
    args: types::AdminAttachVectorIndexShardArgs,
) -> Result<(), RouterError> {
    canister::admin_attach_vector_index_shard(args).await
}

/// Read-only exact `ivf_flat` vector search: composite query that resolves the activated target and
/// forwards to the router-guarded vector canister (ADR 0031 Slice 5). Fails closed unless the
/// Slice 4 activation gate is satisfied.
#[query(composite = true)]
async fn vector_search(
    req: types::RouterVectorSearchRequest,
) -> Result<gleaph_graph_kernel::vector_index::VectorSearchResult, RouterError> {
    canister::vector_search(req).await
}

// --- ADR 0031 Slice 10: Router-forwarded vector maintenance surface (Admin only) ---
//
// Reads are composite queries; mutators/drivers are updates. Each resolves the graph/index to its
// activated vector target and fails closed on missing target/readiness. The vector canister stays
// router-guarded, so these are the only operator entry points.

/// Head-only O(`nlist`) partition-health summary, forwarded to the activated vector target.
#[query(composite = true)]
async fn admin_vector_partition_health(
    graph_name: String,
    index_id: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorPartitionHealthSummary, RouterError> {
    canister::admin_vector_partition_health(graph_name, index_id).await
}

/// Bounded page-meta tombstone-health scan step, forwarded to the activated vector target.
#[query(composite = true)]
async fn admin_vector_partition_health_step(
    graph_name: String,
    index_id: u32,
    cursor: Option<Vec<u8>>,
    max_pages: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorPartitionHealthStep, RouterError> {
    canister::admin_vector_partition_health_step(graph_name, index_id, cursor, max_pages).await
}

/// O(1) rebuild status, forwarded to the activated vector target.
#[query(composite = true)]
async fn admin_vector_rebuild_status(
    graph_name: String,
    index_id: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorRebuildStatus, RouterError> {
    canister::admin_vector_rebuild_status(graph_name, index_id).await
}

/// Derived slab-space observability, forwarded to the graph's vector target (`index_id` scopes the
/// logical counters; the slab physical facts are whole-slab global).
#[query(composite = true)]
async fn admin_vector_slab_stats(
    graph_name: String,
    index_id: Option<u32>,
) -> Result<gleaph_graph_kernel::vector_index::VectorSlabStats, RouterError> {
    canister::admin_vector_slab_stats(graph_name, index_id).await
}

/// Cursor/budgeted slab-stats scan step, forwarded to the graph's vector target.
#[query(composite = true)]
async fn admin_vector_slab_stats_step(
    graph_name: String,
    cursor: Option<Vec<u8>>,
    max_pages: u32,
    index_id: Option<u32>,
) -> Result<gleaph_graph_kernel::vector_index::VectorSlabStatsStep, RouterError> {
    canister::admin_vector_slab_stats_step(graph_name, cursor, max_pages, index_id).await
}

/// Heap centroid cache status, forwarded to the graph's vector target.
#[query(composite = true)]
async fn admin_vector_centroid_cache_status(
    graph_name: String,
) -> Result<gleaph_graph_kernel::vector_index::VectorCentroidCacheStatus, RouterError> {
    canister::admin_vector_centroid_cache_status(graph_name).await
}

/// Vector-canister-owned maintenance execution state, forwarded to the activated vector target.
#[query(composite = true)]
async fn admin_vector_maintenance_status(
    graph_name: String,
    index_id: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorMaintenanceState, RouterError> {
    canister::admin_vector_maintenance_status(graph_name, index_id).await
}

/// Begin a shadow-version rebuild on the activated vector target.
#[update]
async fn admin_start_vector_rebuild(
    graph_name: String,
    index_id: u32,
    nlist: u32,
    sample_limit: u32,
) -> Result<(), RouterError> {
    canister::admin_start_vector_rebuild(graph_name, index_id, nlist, sample_limit).await
}

/// Begin a rebuild only if attested partition health crosses the supplied policy, on the activated
/// vector target.
#[update]
async fn admin_start_vector_rebuild_if_recommended(
    graph_name: String,
    index_id: u32,
    attested_page_health: gleaph_graph_kernel::vector_index::VectorPartitionPageHealth,
    policy: gleaph_graph_kernel::vector_index::VectorMaintenancePolicy,
    target_nlist: Option<u32>,
    sample_limit: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorMaintenanceRecommendation, RouterError> {
    canister::admin_start_vector_rebuild_if_recommended(
        graph_name,
        index_id,
        attested_page_health,
        policy,
        target_nlist,
        sample_limit,
    )
    .await
}

/// Drive one bounded rebuild step on the activated vector target.
#[update]
async fn admin_vector_rebuild_step(
    graph_name: String,
    index_id: u32,
    max_subjects: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorRebuildStatus, RouterError> {
    canister::admin_vector_rebuild_step(graph_name, index_id, max_subjects).await
}

/// Publish a `ReadyToPublish` rebuild on the activated vector target.
#[update]
async fn admin_publish_vector_rebuild(
    graph_name: String,
    index_id: u32,
) -> Result<(), RouterError> {
    canister::admin_publish_vector_rebuild(graph_name, index_id).await
}

/// Abort an in-flight rebuild on the activated vector target.
#[update]
async fn admin_abort_vector_rebuild(graph_name: String, index_id: u32) -> Result<(), RouterError> {
    canister::admin_abort_vector_rebuild(graph_name, index_id).await
}

/// Drive one bounded cleanup/abort teardown step on the activated vector target.
#[update]
async fn admin_vector_rebuild_cleanup_step(
    graph_name: String,
    index_id: u32,
    max_work: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorRebuildStatus, RouterError> {
    canister::admin_vector_rebuild_cleanup_step(graph_name, index_id, max_work).await
}

/// Warm the heap centroid cache on the activated vector target.
#[update]
async fn admin_vector_centroid_cache_warmup(
    graph_name: String,
    index_id: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorCentroidCacheStatus, RouterError> {
    canister::admin_vector_centroid_cache_warmup(graph_name, index_id).await
}

/// Clear the entire heap centroid cache on the graph's vector target.
#[update]
async fn admin_vector_centroid_cache_clear(
    graph_name: String,
) -> Result<gleaph_graph_kernel::vector_index::VectorCentroidCacheStatus, RouterError> {
    canister::admin_vector_centroid_cache_clear(graph_name).await
}

/// Reset the maintenance execution state to `Idle` (incl. `Failed`) on the activated vector target.
/// Does not abort an in-flight rebuild (use `admin_abort_vector_rebuild`) or change Router policy.
#[update]
async fn admin_vector_maintenance_reset(
    graph_name: String,
    index_id: u32,
) -> Result<(), RouterError> {
    canister::admin_vector_maintenance_reset(graph_name, index_id).await
}

// --- ADR 0031 Slice 10: Router-owned maintenance policy catalog + push step ---

/// Create or replace the Router-owned maintenance policy for one vector index (DDL admin).
#[update]
fn admin_set_vector_maintenance_policy(
    args: types::SetVectorMaintenancePolicyArgs,
) -> Result<(), RouterError> {
    canister::admin_set_vector_maintenance_policy(args)
}

/// Disable (but keep) the maintenance policy for one vector index (DDL admin).
#[update]
fn admin_disable_vector_maintenance_policy(
    graph_name: String,
    index_id: u32,
) -> Result<(), RouterError> {
    canister::admin_disable_vector_maintenance_policy(graph_name, index_id)
}

/// Delete the maintenance policy for one vector index (DDL admin). Returns whether one existed.
#[update]
fn admin_delete_vector_maintenance_policy(
    graph_name: String,
    index_id: u32,
) -> Result<bool, RouterError> {
    canister::admin_delete_vector_maintenance_policy(graph_name, index_id)
}

/// The maintenance policy for one vector index, if any.
#[query]
fn vector_maintenance_policy(
    graph_name: String,
    index_id: u32,
) -> Result<Option<types::VectorMaintenancePolicyView>, RouterError> {
    canister::vector_maintenance_policy(graph_name, index_id)
}

/// All maintenance policies in a graph.
#[query]
fn list_vector_maintenance_policies(
    graph_name: String,
) -> Result<Vec<types::VectorMaintenancePolicyView>, RouterError> {
    canister::list_vector_maintenance_policies(graph_name)
}

/// Advance one bounded maintenance unit for an enabled policy; `Disabled` no-op otherwise.
#[update]
async fn admin_vector_maintenance_step(
    graph_name: String,
    index_id: u32,
) -> Result<types::VectorMaintenanceStepOutcome, RouterError> {
    canister::admin_vector_maintenance_step(graph_name, index_id).await
}

/// Router policy/readiness plus forwarded vector-canister maintenance + rebuild state.
#[query(composite = true)]
async fn vector_maintenance_status(
    graph_name: String,
    index_id: u32,
) -> Result<types::VectorMaintenanceStatusView, RouterError> {
    canister::vector_maintenance_status(graph_name, index_id).await
}

/// Advance label posting backfill for one graph shard (`Role::Admin`; call in a loop).
#[update]
async fn admin_label_backfill_step(
    args: types::AdminLabelBackfillStepArgs,
) -> Result<types::AdminLabelBackfillStepResult, RouterError> {
    canister::admin_label_backfill_step(args).await
}

/// Operator recovery: clear a stuck `in_progress` claim on a shard's backfill
/// cursor (`Role::Admin`). Use only when no step is in flight for the shard.
#[update]
fn admin_reset_backfill_claim(args: types::AdminResetBackfillClaimArgs) -> Result<(), RouterError> {
    canister::admin_reset_backfill_claim(args)
}

/// List router-stable backfill cursors for all shards of a logical graph.
#[query]
fn admin_list_label_backfill_status(
    logical_graph_name: String,
) -> Result<Vec<types::LabelBackfillShardStatus>, RouterError> {
    canister::admin_list_label_backfill_status(logical_graph_name)
}

/// Advance vertex property posting backfill for one graph shard (`Role::Admin`; call in a loop).
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

/// Advance edge property posting backfill for one graph shard (`Role::Admin`; call in a loop).
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

/// Advance label stats projection for one graph shard (`Role::Admin`; call in a loop).
#[update]
async fn admin_label_stats_projection_step(
    args: types::AdminLabelStatsProjectionStepArgs,
) -> Result<types::AdminLabelStatsProjectionStepResult, RouterError> {
    canister::admin_label_stats_projection_step(args).await
}

ic_cdk::export_candid!();
