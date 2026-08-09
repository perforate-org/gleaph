//! L1 client data-plane surface (ADR 0056 §3).
//!
//! The audience is applications and the redesigned SDK: GQL read/write, prepared operations,
//! `vector_search`, and mutation status. Graph resolution follows the program (`USE GRAPH`) for
//! the GQL family; explicit graph arguments appear where the current surface already has them.

use ic_cdk_macros::{query, update};

use crate::facade::stable::label_stats::ClientMutationKey;
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
async fn gql_mutate(
    query: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
    gql::gql_execute_idempotent(query, params, client_mutation_key).await
}

/// ADR 0029 Phase 4: pull-based status of a GQL/prepared mutation for the calling principal.
#[query]
fn mutation_status(
    graph_name: Option<String>,
    client_mutation_key: String,
) -> Result<types::MutationStatus, RouterError> {
    let caller = ic_cdk::api::msg_caller();
    let store = crate::facade::store::RouterStore::new();
    mutation_status_for(&store, caller, graph_name.as_deref(), &client_mutation_key)
}

/// Return the exact durable receipt for an ordered atomic insert.
#[query]
fn atomic_insert_status(
    graph_name: Option<String>,
    client_mutation_key: String,
) -> Result<types::AtomicInsertResponse, RouterError> {
    let caller = ic_cdk::api::msg_caller();
    let store = crate::facade::store::RouterStore::new();
    atomic_insert_status_for(&store, caller, graph_name.as_deref(), &client_mutation_key)
}

fn mutation_status_record(
    store: &crate::facade::store::RouterStore,
    caller: candid::Principal,
    graph_name: Option<&str>,
    client_mutation_key: &str,
) -> Result<
    (
        gleaph_graph_kernel::entry::GraphId,
        crate::facade::stable::label_stats::RouterMutationRecord,
    ),
    RouterError,
> {
    let graph_id = crate::graph_context::resolve_graph_id_or_default(store, caller, graph_name)?;
    let key = ClientMutationKey::new(caller, graph_id, client_mutation_key.to_owned());
    let record = store
        .router_mutation_record(&key)
        .ok_or_else(|| RouterError::NotFound(client_mutation_key.to_owned()))?;
    Ok((graph_id, record))
}

fn mutation_status_for(
    store: &crate::facade::store::RouterStore,
    caller: candid::Principal,
    graph_name: Option<&str>,
    client_mutation_key: &str,
) -> Result<types::MutationStatus, RouterError> {
    let (_, record) = mutation_status_record(store, caller, graph_name, client_mutation_key)?;
    record.ensure_gql_mutation_family()?;
    Ok(types::MutationStatus::from_record(&record))
}

fn atomic_insert_status_for(
    store: &crate::facade::store::RouterStore,
    caller: candid::Principal,
    graph_name: Option<&str>,
    client_mutation_key: &str,
) -> Result<types::AtomicInsertResponse, RouterError> {
    let (graph_id, record) =
        mutation_status_record(store, caller, graph_name, client_mutation_key)?;
    record.ensure_atomic_insert_family()?;
    let encoding_key = store.graph_element_id_encoding_key(graph_id)?;
    Ok(types::AtomicInsertResponse::from_record_with_encoding_key(
        &record,
        &encoding_key,
    ))
}

/// ADR 0049: classify and execute one order-preserving public atomic insert.
#[update]
async fn atomic_insert(
    request: types::AtomicInsertRequest,
) -> Result<types::AtomicInsertResponse, RouterError> {
    crate::gql::atomic_insert_public(request).await
}

/// Start, append, finalize, or abort one durable graph-scoped bulk-load job (ADR 0057).
#[update]
async fn bulk_load(
    command: types::BulkLoadCommand,
) -> Result<types::BulkLoadResponse, RouterError> {
    crate::bulk_load::bulk_load_public(command).await
}

/// Return a bounded page of committed durable bulk-load chunk receipts (ADR 0057).
#[query]
fn bulk_load_status(
    graph_name: Option<String>,
    client_bulk_key: String,
    receipt_cursor: Option<u32>,
    max_receipts: u32,
) -> Result<types::BulkLoadStatusPage, RouterError> {
    crate::bulk_load::bulk_load_status_public(
        graph_name,
        client_bulk_key,
        receipt_cursor,
        max_receipts,
    )
}

#[update]
/// Register or replace named prepared operations in one atomic batch (idempotent upsert).
/// Per-operation `metadata` is optional (ADR 0061).
fn prepare(operations: Vec<gleaph_prepared_api::PreparedRegistration>) -> Result<(), RouterError> {
    prepared::prepare(operations)
}

/// Remove one named prepared operation.
#[update]
fn drop_prepared(name: String) -> Result<(), RouterError> {
    prepared::drop_prepared(&name)
}

/// The full prepared-operation manifest for one graph.
#[query]
fn list_prepared(
    graph_name: Option<String>,
) -> Result<gleaph_prepared_api::PreparedManifest, RouterError> {
    prepared::list_prepared(graph_name)
}

/// The stored source and metadata of one registered prepared operation.
#[query]
fn get_prepared(name: String) -> Result<gleaph_prepared_api::PreparedOperationRecord, RouterError> {
    prepared::get_prepared(&name)
}

/// Read-only prepared execution with an explicit ADR 0029 §5 read-consistency contract.
#[query(composite = true)]
async fn prepared_query(
    name: String,
    params: Vec<u8>,
    sort: Option<Vec<gleaph_prepared_api::PreparedSortSpec>>,
    read_mode: gleaph_graph_kernel::plan_exec::ReadMode,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
    prepared::prepared_query(name, params, sort, read_mode).await
}

/// Idempotent prepared update. Returns the richer
/// [`GqlQueryResult`](gleaph_graph_kernel::plan_exec::GqlQueryResult) carrying the ADR 0029
/// federated mutation lifecycle `phase`.
#[update]
async fn prepared_mutate(
    name: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, RouterError> {
    prepared::prepared_mutate(name, params, client_mutation_key).await
}

/// Read-only exact `ivf_flat` vector search: composite query that resolves the named vector index
/// and forwards to the router-guarded vector canister (ADR 0031 Slice 5). Fails closed unless the
/// Slice 4 activation gate is satisfied. `query` is the encoded F32 vector (dims are inferred from
/// the registered index definition).
#[query(composite = true)]
async fn vector_search(
    graph_name: Option<String>,
    index_name: String,
    query: Vec<u8>,
    top_k: u32,
) -> Result<gleaph_graph_kernel::vector_index::VectorSearchResult, RouterError> {
    use crate::facade::stable::{embedding_name_catalog, vector_index_catalog};
    use gleaph_graph_kernel::vector_index::{MAX_VECTOR_SEARCH_TOP_K, VectorSearchRequest};

    let store = crate::facade::store::RouterStore::new();
    let graph_id = match graph_name.as_deref() {
        Some(name) => store.resolve_graph_id(name)?,
        None => crate::graph_context::resolve_default_graph_id(&store, ic_cdk::api::msg_caller())?,
    };
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
    use crate::facade::stable::ROUTER_MUTATION_BY_CLIENT_KEY;
    use crate::facade::stable::graph_catalog::lookup_graph_id;
    use crate::facade::stable::label_stats::{
        ClientMutationKey, RouterMutationPayloadV1, RouterMutationRecord,
        RouterMutationRequestIdentityV1,
    };
    use crate::facade::store::RouterStore;
    use crate::facade::store::tests::graph_type_catalog_vocabulary::{
        register_vector_def, setup_one_shard_graph,
    };
    use crate::facade::store::tests::{graph_principal, register_test_graph, test_init_args};
    use crate::state::RouterError;
    use crate::types::{AdminAttachVectorIndexShardArgs, AtomicInsertReceiptV1, ShardId};
    use candid::Principal;
    use gleaph_graph_kernel::plan_exec::{
        GraphOrderedEdgeBatchReceiptV1, MutationLifecyclePhase, MutationTokenShard,
    };

    // `vector_search` orchestration tests (ADR 0056: relocated from `facade/store/tests.rs` so the
    // api layer owns activation gating, missing index/target, and prevalidation coverage).

    fn router_search_args(index_name: &str, top_k: u32) -> (Option<String>, String, Vec<u8>, u32) {
        (
            Some("tenant.main".into()),
            index_name.into(),
            vec![0u8; 16 * 4],
            top_k,
        )
    }

    fn setup_status_graph() -> (RouterStore, Principal) {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[caller]);
        register_test_graph(&store, caller, "tenant.main");
        (store, caller)
    }

    fn insert_status_record(caller: Principal, client_key: &str, record: RouterMutationRecord) {
        let graph_id = lookup_graph_id("tenant.main").expect("graph id");
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|records| {
            records.insert(
                ClientMutationKey::new(caller, graph_id, client_key.into()),
                record,
            );
        });
    }

    #[test]
    fn status_endpoints_return_exact_not_found_for_missing_key() {
        let (store, caller) = setup_status_graph();
        assert_eq!(
            super::mutation_status_for(&store, caller, Some("tenant.main"), "missing"),
            Err(RouterError::NotFound("missing".into()))
        );
        assert_eq!(
            super::atomic_insert_status_for(&store, caller, Some("tenant.main"), "missing"),
            Err(RouterError::NotFound("missing".into()))
        );
    }

    #[test]
    fn atomic_status_recovers_receipt_and_both_status_gates_reject_wrong_family() {
        let (store, caller) = setup_status_graph();

        let gql_record = RouterMutationRecord::new(1, 0, b"gql".to_vec());
        insert_status_record(caller, "gql", gql_record);
        assert_eq!(
            super::mutation_status_for(&store, caller, Some("tenant.main"), "gql")
                .expect("GQL status")
                .mutation_id,
            1
        );
        assert_eq!(
            super::atomic_insert_status_for(&store, caller, Some("tenant.main"), "gql"),
            Err(RouterError::Conflict(
                "client_mutation_key belongs to a different mutation family".into()
            ))
        );

        let receipt = GraphOrderedEdgeBatchReceiptV1 {
            logical_edge_count: 3,
            emitted_delta_first_seq: None,
            emitted_delta_last_seq: None,
            hot_forward_vertices: Vec::new(),
        };
        let mut atomic_record = RouterMutationRecord::new(2, 0, vec![7; 32]);
        atomic_record.as_v1_mut().request_identity =
            RouterMutationRequestIdentityV1::OrderedEdgeBatch {
                public_fingerprint: [7; 32],
                public_item_count: 3,
            };
        atomic_record.as_v1_mut().routing_in_progress = false;
        atomic_record.as_v1_mut().completed_row_count = Some(3);
        atomic_record.as_v1_mut().payload = RouterMutationPayloadV1::CompletedOrderedEdgeBatch {
            graph_request_fingerprint: [0; 32],
            receipt,
            projection_watermark: MutationTokenShard {
                shard_id: ShardId::new(0),
                label_stats_seq: None,
            },
        };
        insert_status_record(caller, "atomic", atomic_record);

        let recovered =
            super::atomic_insert_status_for(&store, caller, Some("tenant.main"), "atomic")
                .expect("recover atomic receipt after response loss");
        assert_eq!(recovered.status.phase, MutationLifecyclePhase::Completed);
        assert_eq!(
            recovered.receipt,
            Some(AtomicInsertReceiptV1 {
                logical_operation_count: 3,
                logical_vertex_count: 0,
                logical_edge_count: 3,
                allocated_vertex_ids: Vec::new(),
            })
        );
        assert_eq!(
            super::mutation_status_for(&store, caller, Some("tenant.main"), "atomic"),
            Err(RouterError::Conflict(
                "client_mutation_key belongs to a different mutation family".into()
            ))
        );
    }

    #[test]
    fn status_endpoints_reject_inconsistent_identity_and_payload() {
        let (store, caller) = setup_status_graph();
        let mut corrupt = RouterMutationRecord::new(3, 0, vec![8; 32]);
        corrupt.as_v1_mut().request_identity = RouterMutationRequestIdentityV1::OrderedEdgeBatch {
            public_fingerprint: [8; 32],
            public_item_count: 1,
        };
        insert_status_record(caller, "corrupt", corrupt);
        let expected = Err(RouterError::Conflict(
            "mutation record request identity and payload families disagree".into(),
        ));
        assert_eq!(
            super::mutation_status_for(&store, caller, Some("tenant.main"), "corrupt"),
            expected
        );
        assert_eq!(
            super::atomic_insert_status_for(&store, caller, Some("tenant.main"), "corrupt"),
            Err(RouterError::Conflict(
                "mutation record request identity and payload families disagree".into()
            ))
        );
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
            Some("tenant.b".into()),
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
                vector_canister: graph_principal(7),
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
            vec![gleaph_graph_kernel::entry::VertexLabelId::from_raw(1)],
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
            Some("tenant.main".into()),
            "vec1".into(),
            vec![0u8; 16 * 4 - 4],
            10,
        ))
        .expect_err("byte length mismatch");
        assert!(matches!(err, RouterError::InvalidArgument(_)));
    }
}
