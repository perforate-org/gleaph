//! Gleaph router canister — federation control plane (graph registry, shard registry).

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
mod recovery;
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
