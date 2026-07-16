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
mod edge_inline_value_ddl;
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
mod provisioning;
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
pub use init::{RouterInitArgs, RouterUpgradeArgs};
pub use state::RouterError;

use candid::{Encode, Principal};
use ic_cdk_macros::{init, post_upgrade, query, update};

use crate::provisioning::sender::send_accept_envelope;
use crate::types::RouterOutboundError;

const MAX_UPDATE_CALL_INSTRUCTIONS: u64 = 40_000_000_000;
const DYNAMIC_INSTRUCTION_HEADROOM: u64 = 5_000_000_000;
const MAX_DYNAMIC_INSTRUCTION_BUDGET: u64 =
    MAX_UPDATE_CALL_INSTRUCTIONS - DYNAMIC_INSTRUCTION_HEADROOM;
const DEFAULT_DYNAMIC_INSTRUCTION_BUDGET: u64 = MAX_DYNAMIC_INSTRUCTION_BUDGET;

#[cfg(target_family = "wasm")]
fn current_instruction_counter() -> u64 {
    ic_cdk::api::call_context_instruction_counter()
}

#[cfg(not(target_family = "wasm"))]
fn current_instruction_counter() -> u64 {
    0
}

#[init]
fn init(args: RouterInitArgs) {
    canister::init(args);
    // ADR 0029 Phase 4: arm the autonomous saga recovery driver (no-op until there is work).
    recovery::arm_if_needed();
}

#[post_upgrade]
fn post_upgrade(args: Option<RouterUpgradeArgs>) {
    canister::post_upgrade(args.unwrap_or_default());
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

/// Execute cursor-based idempotent mutations until the Router instruction budget is reached.
///
/// Each wave reuses the fixed batch coordinator and is independently partial-successful. A
/// returned `next_index` is the only continuation signal; retrying the same cursor is safe because
/// every item retains its original client mutation key.
#[update]
async fn gql_execute_idempotent_batch(
    args: types::GqlExecuteIdempotentBatchArgs,
) -> Result<types::GqlExecuteIdempotentBatchResult, RouterError> {
    let request_bytes = Encode!(&args).map_err(|error| {
        RouterError::InvalidArgument(format!(
            "gql_execute_idempotent_batch request encode failed: {error}"
        ))
    })?;
    if request_bytes.len() > gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "gql_execute_idempotent_batch request exceeds the safe payload limit of {} bytes",
            gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
        )));
    }
    let total = args.mutations.len() as u32;
    if total == 0 {
        return Err(RouterError::InvalidArgument(
            "gql_execute_idempotent_batch requires mutations".into(),
        ));
    }
    if args.start_index >= total {
        return Err(RouterError::InvalidArgument(format!(
            "start_index {} is outside mutation list of length {total}",
            args.start_index
        )));
    }
    let budget = match args.instruction_budget {
        None => DEFAULT_DYNAMIC_INSTRUCTION_BUDGET,
        Some(value) if value <= MAX_DYNAMIC_INSTRUCTION_BUDGET => value,
        value => {
            return Err(RouterError::InvalidArgument(format!(
                "instruction_budget {:?} exceeds safe maximum {MAX_DYNAMIC_INSTRUCTION_BUDGET}",
                value
            )));
        }
    };

    let mut cursor = args.start_index as usize;
    let end = args.mutations.len();
    let mut results = Vec::new();
    while cursor < end && current_instruction_counter() < budget {
        let coordinator = gql::BatchDispatchCoordinator::new_dynamic(end - cursor, budget);
        let wave_slice = args.mutations[cursor..end]
            .iter()
            .cloned()
            .enumerate()
            .collect::<Vec<_>>();
        let wave_results = {
            let preflight = gql::PreflightContext::new();
            let futures = wave_slice.iter().cloned().map(|(item_index, mutation)| {
                gql::gql_execute_idempotent_with_batch_outcome(
                    mutation.gql_query,
                    mutation.params,
                    mutation.mutation_key,
                    Some((coordinator.clone(), item_index)),
                    Some(&preflight),
                )
            });
            futures::future::join_all(futures).await
        };
        let next_deferred = wave_results
            .iter()
            .position(|result| matches!(result, Ok(None)));
        let next_cursor = next_deferred.map(|offset| cursor + offset);
        let result_limit = next_deferred.unwrap_or(wave_results.len());
        results.extend(
            wave_results
                .into_iter()
                .take(result_limit)
                .map(|result| {
                    result.and_then(|value| {
                        value.ok_or_else(|| {
                            RouterError::InvalidArgument(
                                "unexpected deferred mutation in completed prefix".into(),
                            )
                        })
                    })
                })
                .collect::<Result<Vec<_>, RouterError>>()?,
        );
        if let Some(next_cursor) = next_cursor {
            if next_cursor == cursor {
                return Err(RouterError::InvalidArgument(
                    "instruction budget was exhausted before the next mutation could start; retry"
                        .into(),
                ));
            }
            cursor = next_cursor;
        } else {
            cursor = end;
        }
    }
    if cursor == args.start_index as usize {
        return Err(RouterError::InvalidArgument(
            "instruction budget is already exhausted; increase instruction_budget or retry".into(),
        ));
    }
    let instruction_counter = current_instruction_counter();
    let result = types::GqlExecuteIdempotentBatchResult {
        results,
        next_index: (cursor < args.mutations.len()).then_some(cursor as u32),
        instruction_counter,
    };
    let response_bytes = Encode!(&result).map_err(|error| {
        RouterError::InvalidArgument(format!(
            "gql_execute_idempotent_batch response encode failed: {error}"
        ))
    })?;
    if response_bytes.len() > gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "gql_execute_idempotent_batch response exceeds the safe payload limit of {} bytes",
            gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
        )));
    }
    Ok(result)
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
fn prepared_register_batch(queries: Vec<(String, String)>) -> Vec<Result<(), RouterError>> {
    prepared::prepared_register_batch(queries)
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

/// Admin: ingest one finite F32 vertex embedding through Router into the owning Graph shard
/// (plan 0048). Resolves the opaque graph-scoped vertex id, validates the registered embedding
/// definition, and dispatches a single canonical write. The result reports the canonical embedding
/// version and whether the derived vector projection was applied or deferred for repair.
#[update]
async fn admin_ingest_vertex_embedding(
    args: types::AdminIngestVertexEmbeddingArgs,
) -> Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, RouterError> {
    canister::admin_ingest_vertex_embedding(args).await
}

/// Admin (plan 0048 extension): ingest many finite F32 vertex embeddings in one call. Items are
/// grouped by target graph canister and dispatched in bounded chunks so the social-demo seed pays
/// one Router→Graph call and one Graph→Vector call.
#[update]
async fn admin_ingest_vertex_embedding_batch(
    args: types::AdminIngestVertexEmbeddingBatchArgs,
) -> Result<
    Vec<Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, String>>,
    RouterError,
> {
    canister::admin_ingest_vertex_embedding_batch(args).await
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

/// Admin-only: send a resolved provisioning envelope to the configured Provision canister.
#[update]
async fn provision_graph(
    args: types::ProvisionGraphArgs,
) -> Result<types::ProvisionGraphResponse, RouterError> {
    use crate::facade::auth;
    use crate::facade::store::provisioning::{InsertError, RouterProvisioningRequestStore};
    use crate::types::{
        ProvisionableResourceKind, ProvisioningIntentKey, ProvisioningRequestKey,
        RouterProvisioningRequest, RouterProvisioningRequestState,
    };

    let caller = ic_cdk::api::msg_caller();
    auth::require_admin(&caller)?;

    let provision_canister = crate::provisioning::config::get().ok_or_else(|| {
        RouterError::NotImplemented("provision_canister not configured".to_owned())
    })?;

    // Validate requested_resources non-empty and canonical intent present.
    if args.requested_resources.is_empty() {
        return Err(RouterError::InvalidArgument(
            "requested_resources is empty".to_owned(),
        ));
    }
    let canonical = args
        .requested_resources
        .iter()
        .find(|r| r.kind == ProvisionableResourceKind::GraphShard)
        .ok_or_else(|| {
            RouterError::InvalidArgument(
                "requested_resources must contain at least one GraphShard resource".to_owned(),
            )
        })?;
    let intent_key = ProvisioningIntentKey::new(
        &args.deployment_id,
        canonical.kind,
        &canonical.logical_resource_key,
    );

    // Seed the Router-side provisioning-request catalog before the outbound send so the
    // ack callback has a canonical record to advance. We need deployment_id for the key, so
    // clone it before moving fields into the ProvisionRequest wire struct.
    let deployment_id = args.deployment_id.clone();
    let request_id = format!("{}-{}", args.graph_name, args.request_fingerprint);
    let request_key = ProvisioningRequestKey::new(&request_id, &deployment_id);
    let store = RouterProvisioningRequestStore::new();
    let seed_record = RouterProvisioningRequest {
        request_id: request_id.clone(),
        request_fingerprint: args.request_fingerprint.clone(),
        caller: ic_cdk::api::msg_caller(),
        graph_name: args.graph_name.clone(),
        reserved_graph_id: None,
        requested_resources: args.requested_resources.clone(),
        state: RouterProvisioningRequestState::AwaitingAck,
        provision_receipt: None,
        accepted_registry_version: None,
        created_at_ns: ic_cdk::api::time(),
    };
    let outcome = store
        .insert(&deployment_id, seed_record)
        .map_err(|err| match err {
            InsertError::Conflict => {
                RouterError::Conflict("provisioning request fingerprint conflict".to_owned())
            }
            InsertError::IntentConflict => {
                RouterError::Conflict("provisioning intent already locked".to_owned())
            }
            InsertError::InvalidDuplicateIntent => {
                RouterError::InvalidArgument("duplicate requested resources".to_owned())
            }
        })?;

    let request = gleaph_graph_kernel::provisioning::wire::ProvisionRequest {
        deployment_id,
        request_id,
        request_fingerprint: args.request_fingerprint,
        intent_key,
        reserved_graph_id: None,
        graph_name: args.graph_name,
        requested_resources: args.requested_resources,
        authorized_caller: args.authorized_caller,
        release_id: args.release_id,
        // Sender will overwrite this with ic_cdk::api::canister_self() before encoding.
        router_callback_principal: candid::Principal::anonymous(),
    };

    dispatch_provision_send(request_key, outcome, store, || {
        send_accept_envelope(provision_canister, request)
    })
    .await
}

/// Maps a Provision outbound error to the Router ingress error returned by `provision_graph`.
fn map_provision_outbound_error(err: RouterOutboundError) -> RouterError {
    match err {
        RouterOutboundError::CallFailed(s) => RouterError::ProvisionCallFailed(s),
        RouterOutboundError::UnknownDeployment => {
            RouterError::UnknownDeployment("deployment not bound".to_owned())
        }
        RouterOutboundError::Conflict => RouterError::ProvisionConflict("conflict".to_owned()),
        RouterOutboundError::IngressRejected(s) => RouterError::ProvisionRejected(s),
        RouterOutboundError::EncodingFailed(s) => RouterError::ProvisionEncodingFailed(s),
    }
}

/// Maps a successful `accept_envelope` response to the Router ingress response.
fn build_provision_graph_response(
    accept_response: gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse,
) -> types::ProvisionGraphResponse {
    match accept_response {
        gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse::Accepted {
            job_view,
            intent_lock_count,
        } => types::ProvisionGraphResponse::Accepted {
            job_view,
            intent_lock_count,
        },
        gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse::Replay {
            job_view,
            intent_lock_count,
        } => types::ProvisionGraphResponse::Replay {
            job_view,
            intent_lock_count,
        },
    }
}

/// Dispatches the outbound `accept_envelope` send according to the `InsertionOutcome`.
///
/// Four branches:
/// 1. `Inserted(AwaitingAck)` or `Existing(AwaitingAck)` → call `send`. On failure,
///    rollback ONLY if the current operation inserted the record.
/// 2. `Existing(Completed)` → do not resend; return the durable accepted version.
/// 3. `Existing(Pending | Submitted | Failed)` → reject as `InvalidState`.
async fn dispatch_provision_send<F, Fut>(
    request_key: types::ProvisioningRequestKey,
    outcome: crate::facade::store::provisioning::InsertionOutcome,
    store: crate::facade::store::provisioning::RouterProvisioningRequestStore,
    send: F,
) -> Result<types::ProvisionGraphResponse, RouterError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<
            Output = Result<
                gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse,
                RouterOutboundError,
            >,
        >,
{
    use crate::facade::store::provisioning::InsertionOutcome;
    use types::RouterProvisioningRequestState;

    let is_inserted = matches!(outcome, InsertionOutcome::Inserted(_));

    match &outcome {
        InsertionOutcome::Inserted(record) | InsertionOutcome::Existing(record)
            if matches!(record.state, RouterProvisioningRequestState::AwaitingAck) =>
        {
            let accept_response = match send().await {
                Ok(response) => response,
                Err(e) => {
                    if is_inserted {
                        // Invocation-owned rollback: only remove the record if the current
                        // operation inserted it AND it is still in AwaitingAck. Pre-existing
                        // records from any prior invocation must survive a transient send
                        // failure on a retry.
                        store.rollback_if_inserted_and_awaiting(&request_key, &outcome);
                    }
                    return Err(map_provision_outbound_error(e));
                }
            };
            Ok(build_provision_graph_response(accept_response))
        }
        InsertionOutcome::Existing(record)
            if matches!(record.state, RouterProvisioningRequestState::Completed) =>
        {
            let version = record.accepted_registry_version.ok_or_else(|| {
                RouterError::InvalidState(
                    "completed record missing accepted_registry_version".to_owned(),
                )
            })?;
            Ok(types::ProvisionGraphResponse::Completed {
                accepted_registry_version: version,
            })
        }
        InsertionOutcome::Existing(record) => Err(RouterError::InvalidState(format!(
            "request in non-terminal state {:?}",
            record.state
        ))),
        // `Inserted` for a non-AwaitingAck state is impossible because `insert` always seeds
        // `AwaitingAck`; kept as a defensive match arm.
        InsertionOutcome::Inserted(record) => Err(RouterError::InvalidState(format!(
            "freshly inserted request in unexpected state {:?}",
            record.state
        ))),
    }
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

ic_cdk::export_candid!();

#[cfg(test)]
mod provision_graph_tests {
    use candid::Principal;
    use gleaph_graph_kernel::provisioning::wire::{ProvisionAcceptResponse, ProvisionJobSummary};

    use crate::facade::store::provisioning::{InsertionOutcome, RouterProvisioningRequestStore};
    use crate::types::{
        ProvisionGraphResponse, ProvisionableResource, ProvisionableResourceKind,
        ProvisioningRequestKey, RouterOutboundError, RouterProvisioningRequest,
        RouterProvisioningRequestState,
    };

    fn sample_record(
        request_id: &str,
        _deployment_id: &str,
        fingerprint: &str,
        state: RouterProvisioningRequestState,
        version: Option<u64>,
    ) -> RouterProvisioningRequest {
        RouterProvisioningRequest {
            request_id: request_id.to_owned(),
            request_fingerprint: fingerprint.to_owned(),
            caller: Principal::anonymous(),
            graph_name: "tenant.main".to_owned(),
            reserved_graph_id: None,
            requested_resources: vec![ProvisionableResource {
                kind: ProvisionableResourceKind::GraphShard,
                logical_resource_key: "shard-0".to_owned(),
            }],
            state,
            provision_receipt: None,
            accepted_registry_version: version,
            created_at_ns: 0,
        }
    }

    fn job_view() -> ProvisionJobSummary {
        ProvisionJobSummary {
            request_id: "req".to_owned(),
            deployment_id: "deploy".to_owned(),
            state: "AwaitingAck".to_owned(),
            active_resource_index: 0,
            completed_effect_count: 0,
            accepted_registry_version: None,
        }
    }

    fn store() -> RouterProvisioningRequestStore {
        RouterProvisioningRequestStore::new()
    }

    #[test]
    fn existing_completed_does_not_resend_and_returns_version() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-completed";
            let request_id = "req-completed";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-completed",
                RouterProvisioningRequestState::Completed,
                Some(7),
            );
            s.insert(deployment_id, record.clone())
                .expect("insert completed");

            let request_key = ProvisioningRequestKey::new(request_id, deployment_id);
            let outcome = InsertionOutcome::Existing(record);

            let result = super::dispatch_provision_send(
                request_key.clone(),
                outcome,
                s,
                // Sender must not be called for a Completed record.
                || async { panic!("send must not be called for Completed record") },
            )
            .await
            .expect("completed returns ok");

            assert_eq!(
                result,
                ProvisionGraphResponse::Completed {
                    accepted_registry_version: 7
                }
            );
            let stored = store()
                .get_by_request_id(&request_key)
                .expect("record survives");
            assert_eq!(stored.state, RouterProvisioningRequestState::Completed);
        });
    }

    #[test]
    fn existing_awaiting_ack_keeps_record_on_send_failure() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-existing-awaiting";
            let request_id = "req-existing-awaiting";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-existing-awaiting",
                RouterProvisioningRequestState::AwaitingAck,
                None,
            );
            s.insert(deployment_id, record.clone())
                .expect("insert awaiting");

            let request_key = ProvisioningRequestKey::new(request_id, deployment_id);
            let outcome = InsertionOutcome::Existing(record);

            let result =
                super::dispatch_provision_send(request_key.clone(), outcome, s, || async {
                    Err(RouterOutboundError::CallFailed("simulated".to_owned()))
                })
                .await;

            assert!(
                matches!(result, Err(super::RouterError::ProvisionCallFailed(_))),
                "expected ProvisionCallFailed, got {result:?}"
            );
            let stored = store()
                .get_by_request_id(&request_key)
                .expect("record survives");
            assert_eq!(stored.state, RouterProvisioningRequestState::AwaitingAck);
        });
    }

    #[test]
    fn existing_pending_returns_invalid_state() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-pending";
            let request_id = "req-pending";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-pending",
                RouterProvisioningRequestState::Pending,
                None,
            );
            s.insert(deployment_id, record).expect("insert pending");

            let request_key = ProvisioningRequestKey::new(request_id, deployment_id);
            let outcome = InsertionOutcome::Existing(s.get_by_request_id(&request_key).unwrap());

            let result = super::dispatch_provision_send(request_key, outcome, s, || async {
                panic!("send must not be called for non-terminal record")
            })
            .await;

            assert!(
                matches!(result, Err(super::RouterError::InvalidState(_))),
                "expected InvalidState, got {result:?}"
            );
        });
    }

    #[test]
    fn existing_failed_returns_invalid_state() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-failed";
            let request_id = "req-failed";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-failed",
                RouterProvisioningRequestState::Failed {
                    reason: "boom".to_owned(),
                },
                None,
            );
            s.insert(deployment_id, record.clone())
                .expect("insert failed");

            let request_key = ProvisioningRequestKey::new(request_id, deployment_id);
            let outcome = InsertionOutcome::Existing(record);

            let result = super::dispatch_provision_send(request_key, outcome, s, || async {
                panic!("send must not be called for non-terminal record")
            })
            .await;

            assert!(
                matches!(result, Err(super::RouterError::InvalidState(_))),
                "expected InvalidState, got {result:?}"
            );
        });
    }

    #[test]
    fn inserted_awaiting_ack_rolls_back_on_send_failure() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-fresh-awaiting";
            let request_id = "req-fresh-awaiting";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-fresh-awaiting",
                RouterProvisioningRequestState::AwaitingAck,
                None,
            );
            let outcome = InsertionOutcome::Inserted(record);
            let request_key = ProvisioningRequestKey::new(request_id, deployment_id);

            let result =
                super::dispatch_provision_send(request_key.clone(), outcome, s, || async {
                    Err(RouterOutboundError::CallFailed("simulated".to_owned()))
                })
                .await;

            assert!(
                matches!(result, Err(super::RouterError::ProvisionCallFailed(_))),
                "expected ProvisionCallFailed, got {result:?}"
            );
            assert!(store().get_by_request_id(&request_key).is_none());
        });
    }

    #[test]
    fn inserted_awaiting_ack_returns_accepted_on_send_success() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-fresh-success";
            let request_id = "req-fresh-success";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-fresh-success",
                RouterProvisioningRequestState::AwaitingAck,
                None,
            );
            let outcome = InsertionOutcome::Inserted(record);
            let request_key = ProvisioningRequestKey::new(request_id, deployment_id);

            let result = super::dispatch_provision_send(request_key, outcome, s, || async {
                Ok(ProvisionAcceptResponse::Accepted {
                    job_view: job_view(),
                    intent_lock_count: 1,
                })
            })
            .await
            .expect("fresh send succeeds");

            assert!(
                matches!(result, ProvisionGraphResponse::Accepted { .. }),
                "expected Accepted, got {result:?}"
            );
        });
    }
}
