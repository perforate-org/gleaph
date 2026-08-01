//! L1 client data-plane surface (ADR 0056 §3).
//!
//! The audience is applications and the redesigned SDK: GQL read/write, prepared operations,
//! `vector_search`, and mutation status. Graph resolution follows the program (`USE GRAPH`) for
//! the GQL family; explicit graph arguments appear where the current surface already has them.

use candid::Encode;
use ic_cdk_macros::{query, update};

use crate::current_instruction_counter;
use crate::gql;
use crate::prepared;
use crate::state::RouterError;
use crate::types;

/// Read-only GQL: composite query (calls index + graph query endpoints) with an explicit
/// ADR 0029 §5 read-consistency contract. `Eventual` is the default contract; `AtLeast(token)`
/// enforces a retryable read-your-writes barrier against the token's per-shard watermarks.
#[query(composite = true)]
async fn gql_query(
    query: String,
    params: Vec<u8>,
    read_mode: gleaph_graph_kernel::plan_exec::ReadMode,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
    gql::gql_query(query, params, read_mode).await
}

/// Idempotent GQL update. Reuse `client_mutation_key` only for retries of the same mutation.
///
/// Returns the richer [`GqlQueryResult`](gleaph_graph_kernel::plan_exec::GqlQueryResult) so
/// clients can read the ADR 0029 federated mutation lifecycle `phase`, distinguishing a
/// durable canonical commit from full cross-canister projection convergence.
#[update]
async fn gql_execute(
    query: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
    gql::gql_execute_idempotent(query, params, client_mutation_key).await
}

#[cfg(feature = "batch-instr-log")]
fn log_batch_phase(phase: &str, cost: u64) {
    crate::instr_log::push(format!("GLEAPH_ROUTER_BATCH phase={} cost={}", phase, cost));
}

#[cfg(not(feature = "batch-instr-log"))]
#[allow(dead_code)]
#[inline]
fn log_batch_phase(_phase: &str, _cost: u64) {}

/// Execute cursor-based idempotent mutations until the Router instruction budget is reached.
///
/// Mutations are prepared and executed sequentially within one ingress. A returned `next_index`
/// is the only continuation signal; retrying the same cursor is safe because every item retains
/// its original client mutation key.
#[update]
#[allow(unused_variables, unused_assignments)]
async fn gql_execute_batch(
    args: types::GqlExecuteIdempotentBatchArgs,
) -> Result<types::GqlExecuteIdempotentBatchResult, RouterError> {
    let request_bytes = Encode!(&args).map_err(|error| {
        RouterError::InvalidArgument(format!("gql_execute_batch request encode failed: {error}"))
    })?;
    if request_bytes.len() > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "gql_execute_batch request exceeds the safe payload limit of {} bytes",
            gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
        )));
    }
    let total = args.mutations.len() as u32;
    if total == 0 {
        return Err(RouterError::InvalidArgument(
            "gql_execute_batch requires mutations".into(),
        ));
    }
    if args.start_index >= total {
        return Err(RouterError::InvalidArgument(format!(
            "start_index {} is outside mutation list of length {total}",
            args.start_index
        )));
    }
    let budget = match args.instruction_budget {
        None => gleaph_graph_kernel::MAX_DYNAMIC_UPDATE_INSTRUCTIONS,
        Some(value) if value <= gleaph_graph_kernel::MAX_DYNAMIC_UPDATE_INSTRUCTIONS => value,
        value => {
            return Err(RouterError::InvalidArgument(format!(
                "instruction_budget {:?} exceeds safe maximum {}",
                value,
                gleaph_graph_kernel::MAX_DYNAMIC_UPDATE_INSTRUCTIONS
            )));
        }
    };

    let start_cursor = args.start_index as usize;
    let mut cursor = start_cursor;
    let end = args.mutations.len();
    let mut results = Vec::new();
    #[cfg(feature = "batch-instr-log")]
    let ingress_start_instr = current_instruction_counter();
    let preflight = gql::PreflightContext::new();
    let caller = ic_cdk::api::msg_caller();
    while cursor < end {
        let stop_threshold =
            budget.saturating_sub(gleaph_graph_kernel::ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM);
        if current_instruction_counter() >= stop_threshold {
            break;
        }
        let mutation = &args.mutations[cursor];

        // ADR 0044: try to coalesce consecutive mutations that share the same query plan into
        // one bulk group with a single mutation id / saga record.
        let mut group_end = cursor + 1;
        while group_end < end && args.mutations[group_end].gql_query == mutation.gql_query {
            group_end += 1;
        }
        let mut bulk_group_end = group_end;
        let mut bulk_applied = false;
        let mut stop_after_bulk = false;
        while bulk_group_end - cursor >= 2 {
            let group_key = format!(
                "{}#bulk-{}-{}",
                mutation.mutation_key, cursor, bulk_group_end
            );
            match gql::execute_bulk_group(
                caller,
                &group_key,
                &args.mutations[cursor..bulk_group_end],
                gleaph_graph_kernel::plan_exec::GqlExecutionMode::Update,
                Some(&preflight),
            )
            .await?
            {
                gql::BulkGroupExecution::Applied(bulk_results) => {
                    results.extend(bulk_results);
                    cursor = bulk_group_end;
                    bulk_applied = true;
                    stop_after_bulk = bulk_group_end < group_end;
                    break;
                }
                gql::BulkGroupExecution::Unsupported => break,
                gql::BulkGroupExecution::SharedRequestTooLarge => {
                    bulk_group_end = cursor + (bulk_group_end - cursor).div_ceil(2);
                }
            }
        }
        if bulk_applied {
            if stop_after_bulk {
                break;
            }
            continue;
        }

        let result = gql::gql_execute_idempotent_with_batch_outcome(
            mutation.gql_query.clone(),
            mutation.params.clone(),
            mutation.mutation_key.clone(),
            Some(&preflight),
        )
        .await?;
        let result = result.ok_or_else(|| {
            RouterError::InvalidArgument(
                "unexpected deferred mutation in sequential batch ingress".into(),
            )
        })?;
        results.push(result);
        cursor += 1;
    }
    if cursor == start_cursor {
        return Err(RouterError::InvalidArgument(
            "instruction budget is already exhausted; increase instruction_budget or retry".into(),
        ));
    }
    #[cfg(feature = "batch-instr-log")]
    {
        let ingress_total = current_instruction_counter().saturating_sub(ingress_start_instr);
        log_batch_phase(
            &format!("ingress_summary items={} total", cursor - start_cursor),
            ingress_total,
        );
    }
    let instruction_counter = current_instruction_counter();
    let result = types::GqlExecuteIdempotentBatchResult {
        results,
        next_index: (cursor < args.mutations.len()).then_some(cursor as u32),
        instruction_counter,
    };
    let response_bytes = Encode!(&result).map_err(|error| {
        RouterError::InvalidArgument(format!("gql_execute_batch response encode failed: {error}"))
    })?;
    if response_bytes.len() > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "gql_execute_batch response exceeds the safe payload limit of {} bytes",
            gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
        )));
    }
    Ok(result)
}

/// ADR 0029 Phase 4: pull-based status of a federated mutation for the calling principal.
#[query]
fn get_mutation_status(
    logical_graph_name: String,
    client_mutation_key: String,
) -> Result<types::MutationStatus, RouterError> {
    let caller = ic_cdk::api::msg_caller();
    let store = crate::facade::store::RouterStore::new();
    let graph_id = store.resolve_graph_id_authorized(&logical_graph_name, caller)?;
    let record = store
        .router_mutation_record(caller, graph_id, &client_mutation_key)
        .ok_or_else(|| {
            RouterError::InvalidArgument(
                "no mutation found for this client_mutation_key".to_string(),
            )
        })?;
    Ok(types::MutationStatus::from_record(&record))
}

/// ADR 0049: classify and execute one order-preserving public batch.
#[update]
async fn batch_insert(request: types::BatchRequest) -> Result<types::BatchResponse, RouterError> {
    crate::gql::batch_public(request).await
}

#[update]
/// Register or replace one named prepared operation (idempotent upsert). `metadata` is optional.
fn prepare(
    name: String,
    query: String,
    metadata: Option<gleaph_prepared_api::PreparedOperation>,
) -> Result<(), RouterError> {
    prepared::prepare(name, query, metadata)
}

/// Remove one named prepared operation.
#[update]
fn drop_prepared(name: String) -> Result<(), RouterError> {
    prepared::drop_prepared(&name)
}

/// The full prepared-operation manifest for one graph.
#[query]
fn list_prepared(graph_name: String) -> Result<gleaph_prepared_api::PreparedManifest, RouterError> {
    prepared::list_prepared(graph_name)
}

/// Read-only prepared execution with an explicit ADR 0029 §5 read-consistency contract.
#[query(composite = true)]
async fn execute_prepared(
    name: String,
    params: Vec<u8>,
    sort: Option<Vec<gleaph_prepared_api::PreparedSortSpec>>,
    read_mode: gleaph_graph_kernel::plan_exec::ReadMode,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
    prepared::execute_prepared(name, params, sort, read_mode).await
}

/// Idempotent prepared update. Returns the richer
/// [`GqlQueryResult`](gleaph_graph_kernel::plan_exec::GqlQueryResult) carrying the ADR 0029
/// federated mutation lifecycle `phase`.
#[update]
async fn execute_prepared_update(
    name: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
    prepared::execute_prepared_update(name, params, client_mutation_key).await
}

/// Read-only exact `ivf_flat` vector search: composite query that resolves the named vector index
/// and forwards to the router-guarded vector canister (ADR 0031 Slice 5). Fails closed unless the
/// Slice 4 activation gate is satisfied. `query` is the encoded F32 vector (dims are inferred from
/// the registered index definition).
#[query(composite = true)]
async fn vector_search(
    graph_name: String,
    index_name: String,
    query: Vec<u8>,
    top_k: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorSearchResult, RouterError> {
    use crate::facade::stable::{embedding_name_catalog, vector_index_catalog};
    use gleaph_graph_kernel::vector_index::{MAX_VECTOR_SEARCH_TOP_K, VectorSearchRequest};

    let store = crate::facade::store::RouterStore::new();
    let graph_id = store.resolve_graph_id(&graph_name)?;
    let embedding_name_id = embedding_name_catalog::lookup_embedding_name_id(graph_id, &index_name)
        .ok_or_else(|| {
            RouterError::NotFound(format!("vector index/embedding name {index_name}"))
        })?;
    let def = vector_index_catalog::list_vector_indexes(graph_id)
        .into_iter()
        .find(|d| d.embedding_name_id == embedding_name_id)
        .ok_or_else(|| {
            RouterError::NotFound(format!("vector index for embedding name {index_name}"))
        })?;
    // Prevalidate the public request against the Router-owned definition so user mistakes surface as
    // `InvalidArgument`, not as an opaque `Internal` from the downstream vector canister.
    if top_k == 0 || top_k > MAX_VECTOR_SEARCH_TOP_K {
        return Err(RouterError::InvalidArgument(format!(
            "top_k must be in 1..={MAX_VECTOR_SEARCH_TOP_K}"
        )));
    }
    let expected_bytes = def.encoding.stride_bytes(def.dims) as usize;
    if query.len() != expected_bytes {
        return Err(RouterError::InvalidArgument(format!(
            "query byte length {} does not match dims*stride {}",
            query.len(),
            expected_bytes
        )));
    }
    let target = def
        .target
        .ok_or_else(|| {
            RouterError::Conflict(format!("vector index {index_name} has no target set"))
        })?
        .canister;
    // Fail closed on the dynamic gate (global flag + per-graph shard vector-attach to this target).
    vector_index_catalog::assert_vector_search_dispatch_ready(graph_id, &store, &def)?;
    let search = VectorSearchRequest {
        index_id: def.index_id,
        query,
        encoding: def.encoding,
        dims: def.dims,
        metric: def.metric,
        top_k,
        candidate_subjects: None,
    };
    crate::vector_sync::vector_search(target, search)
        .await
        .map_err(RouterError::Internal)
}

#[cfg(test)]
mod tests {
    use crate::facade::store::RouterStore;
    use crate::facade::store::tests::graph_type_catalog_vocabulary::{
        register_vector_def, setup_one_shard_graph,
    };
    use crate::facade::store::tests::{graph_principal, register_test_graph, test_init_args};
    use crate::state::RouterError;
    use crate::types::{AdminAttachVectorIndexShardArgs, ShardId};
    use candid::Principal;

    // `vector_search` orchestration tests (ADR 0056: relocated from `facade/store/tests.rs` so the
    // api layer owns activation gating, missing index/target, and prevalidation coverage).

    fn router_search_args(index_name: &str, top_k: u32) -> (String, String, Vec<u8>, u32) {
        (
            "tenant.main".into(),
            index_name.into(),
            vec![0u8; 16 * 4],
            top_k,
        )
    }

    #[test]
    fn router_vector_search_index_name_is_graph_scoped() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        let graph_a = setup_one_shard_graph(&store, admin);
        // register_vector_def interns the embedding name "vec1" under graph A only.
        register_vector_def(graph_a, 1, graph_principal(7));

        // A second graph exists but never registers the name: the same name must not resolve
        // from it. A global (graph-agnostic) name lookup would wrongly succeed here.
        register_test_graph(&store, admin, "tenant.b");
        let err = futures::executor::block_on(super::vector_search(
            "tenant.b".into(),
            "vec1".into(),
            vec![0u8; 16 * 4],
            10,
        ))
        .expect_err("name must not leak across graphs");
        assert!(matches!(err, RouterError::NotFound(_)));
    }

    #[test]
    fn router_vector_search_blocks_until_activation_ready() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        let graph_id = setup_one_shard_graph(&store, admin);
        register_vector_def(graph_id, 1, graph_principal(7));

        // Activation off: the public read path fails closed even though a target exists.
        let (graph, name, query, top_k) = router_search_args("vec1", 10);
        let err = futures::executor::block_on(super::vector_search(graph, name, query, top_k))
            .expect_err("blocked while not activated");
        assert!(matches!(
            err,
            RouterError::VectorDispatchActivationBlocked(_)
        ));

        // Enable the global flag and vector-attach the shard to the def target -> gate satisfied. The
        // native `vector_sync` stub returns empty hits; the point is that gating passed and the call
        // needs no graph shard-local state beyond the readiness gate.
        crate::facade::stable::vector_activation::set_vector_dispatch_globally_enabled(true);
        futures::executor::block_on(store.admin_attach_vector_index_shard(
            admin,
            AdminAttachVectorIndexShardArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                vector_index_canister: graph_principal(7),
            },
        ))
        .expect("attach vector index shard");
        let (graph, name, query, top_k) = router_search_args("vec1", 10);
        let result = futures::executor::block_on(super::vector_search(graph, name, query, top_k))
            .expect("ready search routes to target");
        assert!(result.hits.is_empty());
    }

    #[test]
    fn router_vector_search_rejects_missing_index_and_target() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        let graph_id = setup_one_shard_graph(&store, admin);

        // No definition for the requested index name -> NotFound.
        let (graph, name, query, top_k) = router_search_args("vec9", 10);
        let err = futures::executor::block_on(super::vector_search(graph, name, query, top_k))
            .expect_err("missing index");
        assert!(matches!(err, RouterError::NotFound(_)));

        // A registered definition with no target can never dispatch -> Conflict.
        let name_id =
            crate::facade::stable::embedding_name_catalog::intern_embedding_name(graph_id, "vec5")
                .expect("intern embedding name");
        crate::facade::stable::vector_index_catalog::register_vector_index(
            graph_id,
            5,
            name_id,
            gleaph_graph_kernel::vector_index::VectorIndexKind::IvfFlat,
            gleaph_graph_kernel::vector_index::VectorMetric::L2Squared,
            gleaph_graph_kernel::vector_index::VectorEncoding::F32,
            16,
            None,
            false,
        )
        .expect("register targetless def");
        let (graph, name, query, top_k) = router_search_args("vec5", 10);
        let err = futures::executor::block_on(super::vector_search(graph, name, query, top_k))
            .expect_err("no target");
        assert!(matches!(err, RouterError::Conflict(_)));
    }

    #[test]
    fn router_vector_search_prevalidates_request_shape() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        let graph_id = setup_one_shard_graph(&store, admin);
        // register_vector_def registers dims = 16, so the F32 stride is 64 bytes.
        register_vector_def(graph_id, 1, graph_principal(7));

        // top_k = 0 -> InvalidArgument (not a downstream Internal).
        let (graph, name, query, top_k) = router_search_args("vec1", 0);
        let err = futures::executor::block_on(super::vector_search(graph, name, query, top_k))
            .expect_err("top_k 0");
        assert!(matches!(err, RouterError::InvalidArgument(_)));

        // Wrong byte length (dims are inferred from the def) -> InvalidArgument.
        let err = futures::executor::block_on(super::vector_search(
            "tenant.main".into(),
            "vec1".into(),
            vec![0u8; 16 * 4 - 4],
            10,
        ))
        .expect_err("byte length mismatch");
        assert!(matches!(err, RouterError::InvalidArgument(_)));
    }
}
