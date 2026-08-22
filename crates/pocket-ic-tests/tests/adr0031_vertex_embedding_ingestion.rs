//! PocketIC contract for plan 0048 canonical vertex-embedding ingestion through Router.
//!
//! Proves that an authorized caller can write one finite F32 embedding via Router using only the
//! graph name, opaque encoded vertex id, embedding name, and values; that the vector canister owns
//! the only durable embedding bytes; that Graph validates the metadata stamp and canonical vertex
//! facts; that the derived vector-index converges without direct vector-canister seeding; and that
//! invalid inputs fail closed before any Graph call.

use candid::{Decode, Encode, Principal};
use gleaph_gql::Value;
use gleaph_gql_ic::GqlWireRows;
use gleaph_graph_kernel::federation::{
    ElementIdEncodingKey, GlobalVertexId, RouterError, ShardId, encode_global_vertex_id,
};
use gleaph_graph_kernel::vector_index::{
    VectorCentroidCacheStatus, VectorEmbeddingSyncOp, VectorEncoding, VectorMetric, VectorSubject,
    VectorSyncBatchOutcome, VectorSyncBatchUnavailable, VertexEmbeddingProjectionOutcome,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, arm_router_fault, drain_maintenance_via_timer,
    e2e_insert_vertex_with_label, ensure_user_graph_type, gql_mutate_as_admin,
    gql_mutate_result_as_admin, gql_query_with_params_as_admin, install_single_shard_federation,
    install_vector_canister, run_router_recovery_timer, user_vertex_label_id, wasm_bytes,
};
use gleaph_router::types::{
    AdminAttachVectorIndexShardArgs, AdminIngestVertexEmbeddingBatchArgs,
    AdminIngestVertexEmbeddingBatchItem, RegisterVectorIndexArgs, VectorIndexActivationStatus,
};

const EMBEDDING_NAME: &str = "ingest_title_vec";
const INDEX_ID: u32 = 1;
const DIMS: u16 = 16;
const VECTOR_SYNC_REPLY_AFTER_COMMIT_FAULT: u8 = 8;
const FRONTIER_REPLY_AFTER_COMMIT_FAULT: u8 = 9;

fn register_with_encoding(
    env: &FederationEnv,
    vector: Principal,
    encoding: Option<VectorEncoding>,
) {
    let args = RegisterVectorIndexArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        embedding_name: EMBEDDING_NAME.to_string(),
        index_id: INDEX_ID,
        dims: DIMS,
        labels: vec!["User".to_string()],
        metric: Some(VectorMetric::L2Squared),
        encoding,
        target: Some(vector),
        if_not_exists: false,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_register_vector_index",
            Encode!(&args).expect("encode"),
        )
        .expect("register call");
    let created: Result<bool, gleaph_graph_kernel::federation::RouterError> =
        Decode!(&bytes, Result<bool, gleaph_graph_kernel::federation::RouterError>)
            .expect("decode");
    assert!(
        created.expect("register succeeds"),
        "first registration must be newly created"
    );
}

fn register(env: &FederationEnv, vector: Principal) {
    register_with_encoding(env, vector, None);
}

fn set_dispatch_activation(env: &FederationEnv, enabled: bool) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "set_vector_dispatch_enabled",
            Encode!(&enabled).expect("encode"),
        )
        .expect("activation call");
    let _: Result<(), gleaph_graph_kernel::federation::RouterError> =
        Decode!(&bytes, Result<(), gleaph_graph_kernel::federation::RouterError>).expect("decode");
}

fn set_graph_vector_routing(env: &FederationEnv, vector: Principal) {
    let bytes = env
        .pic
        .update_call(
            env.graph_source,
            env.router,
            "admin_set_vector_canister",
            Encode!(&vector).expect("encode"),
        )
        .expect("set graph vector routing");
    let _: Result<(), String> = Decode!(&bytes, Result<(), String>).expect("decode");
}

fn attach_shard_to_vector(
    env: &FederationEnv,
    vector: Principal,
    graph_id: gleaph_graph_kernel::entry::GraphId,
) {
    let bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "admin_attach_shard_canister",
            Encode!(&graph_id, &ShardId::new(0), &env.graph_source).expect("encode"),
        )
        .expect("vector attach");
    let _: Result<(), String> = Decode!(&bytes, Result<(), String>).expect("decode");
}

fn attach_shard(env: &FederationEnv, vector: Principal) {
    let args = AdminAttachVectorIndexShardArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        shard_id: ShardId::new(0),
        vector_canister: vector,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "attach_vector_shard",
            Encode!(&args).expect("encode"),
        )
        .expect("router attach");
    let _: Result<(), gleaph_graph_kernel::federation::RouterError> =
        Decode!(&bytes, Result<(), gleaph_graph_kernel::federation::RouterError>).expect("decode");
}

fn fully_activate(env: &FederationEnv, vector: Principal) {
    set_dispatch_activation(env, true);
    let graph_id = {
        let bytes = env
            .pic
            .query_call(
                env.router,
                env.admin,
                "get_graph_id",
                Encode!(&GRAPH_NAME.to_string()).expect("encode"),
            )
            .expect("lookup graph id");
        let result: Result<gleaph_graph_kernel::entry::GraphId, gleaph_graph_kernel::federation::RouterError> =
            Decode!(&bytes, Result<gleaph_graph_kernel::entry::GraphId, gleaph_graph_kernel::federation::RouterError>)
                .expect("decode");
        result.expect("graph id")
    };
    set_graph_vector_routing(env, vector);
    attach_shard_to_vector(env, vector, graph_id);
    attach_shard(env, vector);
}

fn encode_vertex(env: &FederationEnv, local: u32) -> Vec<u8> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "get_id_encoding_key",
            Encode!(&GRAPH_NAME.to_string()).expect("encode"),
        )
        .expect("encoding key");
    let key: Result<[u8; 16], gleaph_graph_kernel::federation::RouterError> =
        Decode!(&bytes, Result<[u8; 16], gleaph_graph_kernel::federation::RouterError>)
            .expect("decode");
    let key = ElementIdEncodingKey(key.expect("key"));
    let global = GlobalVertexId::new(ShardId::new(0), local);
    encode_global_vertex_id(&key, global).0.to_vec()
}

fn vec_bytes(value: f32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(DIMS as usize * 4);
    for _ in 0..DIMS {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn ingest(
    env: &FederationEnv,
    encoded: Vec<u8>,
    values: Vec<f32>,
) -> gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult {
    let args = AdminIngestVertexEmbeddingBatchArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        embedding_name: EMBEDDING_NAME.to_string(),
        items: vec![AdminIngestVertexEmbeddingBatchItem {
            encoded_vertex_id: encoded,
            values,
        }],
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "ingest_vertex_embeddings",
            Encode!(&args).expect("encode"),
        )
        .expect("ingest call");
    let result: Result<
        Vec<Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, String>>,
        gleaph_graph_kernel::federation::RouterError,
    > = Decode!(
        &bytes,
        Result<
            Vec<Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, String>>,
            gleaph_graph_kernel::federation::RouterError,
        >
    )
    .expect("decode");
    let results = result.expect("ingest succeeds");
    assert_eq!(results.len(), 1, "one item in, one result out");
    results
        .into_iter()
        .next()
        .expect("one result")
        .expect("ingest succeeds")
}

fn vertex_element_id(env: &FederationEnv) -> gleaph_gql_ic::GqlWireValue {
    let result =
        gql_query_with_params_as_admin(env, "MATCH (v) RETURN ELEMENT_ID(v) AS v_id", vec![]);
    assert_eq!(result.row_count, 1, "exactly one vertex");
    let rows_blob = result.rows_blob.expect("rows blob");
    let wire = GqlWireRows::decode_blob(&rows_blob).expect("decode rows");
    assert_eq!(wire.rows.len(), 1);
    let mut columns: std::collections::BTreeMap<String, gleaph_gql_ic::GqlWireValue> = wire
        .rows
        .into_iter()
        .next()
        .expect("one row")
        .columns
        .into_iter()
        .collect();
    columns.remove("v_id").expect("v_id column")
}

fn router_vector_index_status(env: &FederationEnv) -> VectorIndexActivationStatus {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "get_vector_index_status",
            Encode!(&GRAPH_NAME.to_string(), &INDEX_ID).expect("encode vector index status"),
        )
        .expect("router vector index status call");
    Decode!(&bytes, Result<VectorIndexActivationStatus, RouterError>)
        .expect("decode router vector index status")
        .expect("router vector index status succeeds")
}

fn router_vector_centroid_cache_status(env: &FederationEnv) -> VectorCentroidCacheStatus {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "get_vector_centroid_cache",
            Encode!(&GRAPH_NAME.to_string()).expect("encode vector cache status"),
        )
        .expect("router vector cache status call");
    Decode!(&bytes, Result<VectorCentroidCacheStatus, RouterError>)
        .expect("decode router vector cache status")
        .expect("router vector cache status succeeds")
}

fn graph_derived_index_outbox_len(env: &FederationEnv) -> u64 {
    let bytes = env
        .pic
        .query_call(
            env.graph_source,
            env.router,
            "e2e_derived_index_outbox_len",
            Encode!(&()).expect("encode graph outbox length"),
        )
        .expect("graph outbox length call");
    Decode!(&bytes, u64).expect("decode graph outbox length")
}

fn router_vector_hit_count(env: &FederationEnv, value: f32) -> u64 {
    let query = format!(
        "MATCH (d) SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) DISTANCE AS distance RETURN d"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(value)),
    )])
    .expect("encode vector search params");
    gql_query_with_params_as_admin(env, &query, params).row_count
}

fn router_vector_search_subjects(
    env: &FederationEnv,
    value: f32,
) -> Vec<gleaph_gql_ic::GqlWireValue> {
    let query = format!(
        "MATCH (d) SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) DISTANCE AS distance RETURN ELEMENT_ID(d) AS d_id"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(value)),
    )])
    .expect("encode vector search subject params");
    let result = gql_query_with_params_as_admin(env, &query, params);
    if result.row_count == 0 {
        return Vec::new();
    }
    let rows_blob = result.rows_blob.expect("vector search subject rows blob");
    let wire = GqlWireRows::decode_blob(&rows_blob).expect("decode vector search subject rows");
    assert_eq!(wire.rows.len() as u64, result.row_count);
    wire.rows
        .into_iter()
        .map(|row| {
            let columns: std::collections::BTreeMap<String, gleaph_gql_ic::GqlWireValue> =
                row.columns.into_iter().collect();
            columns
                .get("d_id")
                .cloned()
                .expect("d_id column in vector search subject row")
        })
        .collect()
}

fn e2e_unbind_vector_definition_store(env: &FederationEnv, vector: Principal) {
    let bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "e2e_unbind_definition_store",
            Encode!(&()).expect("encode vector definition-store unbind"),
        )
        .expect("vector definition-store unbind call");
    Decode!(&bytes, Result<(), String>)
        .expect("decode vector definition-store unbind")
        .expect("vector definition-store owner was not Ready");
}

fn set_embedding_stamp_response_loss(env: &FederationEnv, armed: bool) {
    env.pic
        .update_call(
            env.graph_source,
            env.router,
            "e2e_arm_embedding_stamp_response_loss",
            Encode!(&armed).expect("encode embedding-stamp response-loss flag"),
        )
        .expect("set embedding-stamp response-loss flag");
}

type RouterVectorIngestProbe = (u64, Vec<(u64, Principal, ShardId, u8)>);

fn router_vector_ingest_probe(env: &FederationEnv) -> RouterVectorIngestProbe {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "test_vector_ingest_probe",
            Encode!(&()).expect("encode test_vector_ingest_probe"),
        )
        .expect("router vector-ingest probe call");
    Decode!(
        &bytes,
        Result<RouterVectorIngestProbe, RouterError>
    )
    .expect("decode test_vector_ingest_probe")
    .expect("test_vector_ingest_probe succeeds")
}

type VectorFrontierProbe = (
    u64,
    u64,
    Option<(u64, bool)>,
    bool,
    Option<(u32, u64, u32, u32, u32)>,
);

fn vector_frontier_probe(
    env: &FederationEnv,
    vector: Principal,
    vertex_id: u32,
) -> VectorFrontierProbe {
    let bytes = env
        .pic
        .query_call(
            vector,
            env.router,
            "e2e_frontier_probe",
            Encode!(&INDEX_ID, &ShardId::new(0), &vertex_id).expect("encode e2e_frontier_probe"),
        )
        .expect("vector frontier probe call");
    Decode!(&bytes, Result<VectorFrontierProbe, String>)
        .expect("decode e2e_frontier_probe")
        .expect("e2e_frontier_probe succeeds")
}

type VectorFrontierReceipt = (u64, Option<(u32, u64)>);

fn vector_frontier_receipt_probe(env: &FederationEnv, vector: Principal) -> VectorFrontierReceipt {
    let bytes = env
        .pic
        .query_call(
            vector,
            env.router,
            "e2e_frontier_receipt_probe",
            Encode!(&()).expect("encode e2e_frontier_receipt_probe"),
        )
        .expect("vector frontier receipt probe call");
    Decode!(&bytes, u64, Option<(u32, u64)>).expect("decode e2e_frontier_receipt_probe")
}

#[test]
fn canonical_ingestion_reaches_router_vector_search_without_direct_seeding() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    ensure_user_graph_type(&env);
    register(&env, vector);
    fully_activate(&env, vector);

    let inserted = e2e_insert_vertex_with_label(&env, env.graph_source, user_vertex_label_id(&env));
    let encoded = encode_vertex(&env, inserted.local_vertex_id);

    let result = ingest(&env, encoded, vec![6.0; DIMS as usize]);
    assert_eq!(result.embedding_version, 1, "first write is version 1");
    assert!(
        matches!(
            result.projection_outcome,
            VertexEmbeddingProjectionOutcome::Applied
        ),
        "projection should be applied on activated index"
    );

    // The catalog snapshot is Router-owned. The cache status follows Router -> vector, so it
    // reaches the vector's router-only guard before the later GQL search does.
    let pre_upgrade_catalog = router_vector_index_status(&env);
    let pre_upgrade_router_call = router_vector_centroid_cache_status(&env);

    // The ingestion path uses the Graph → vector-index sync boundary. The vector canister owns
    // the durable derived state, so an upgrade after the batch has been acknowledged must not
    // erase the searchable embedding.
    env.pic
        .upgrade_canister(
            vector,
            wasm_bytes("VECTOR_INDEX_WASM"),
            Encode!(&()).expect("encode vector upgrade args"),
            None,
        )
        .expect("upgrade vector canister");

    assert_eq!(
        router_vector_index_status(&env),
        pre_upgrade_catalog,
        "vector upgrade must not change the Router vector-index catalog"
    );
    assert_eq!(
        router_vector_centroid_cache_status(&env),
        pre_upgrade_router_call,
        "the vector canister must still authorize its configured Router after upgrade"
    );

    let expected_id = vertex_element_id(&env);
    let query = format!(
        "MATCH (d) SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) DISTANCE AS distance RETURN ELEMENT_ID(d) AS d_id, distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(6.0)),
    )])
    .expect("encode params");
    let gql = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(gql.row_count, 1, "exact GQL search returns one row");
    let rows_blob = gql.rows_blob.expect("rows blob");
    let wire = GqlWireRows::decode_blob(&rows_blob).expect("decode rows");
    assert_eq!(wire.rows.len(), 1);
    let columns: std::collections::BTreeMap<String, gleaph_gql_ic::GqlWireValue> =
        wire.rows[0].columns.clone().into_iter().collect();
    assert_eq!(
        columns.get("d_id").expect("d_id column"),
        &expected_id,
        "GQL must return the ingested vertex ELEMENT_ID"
    );
    let distance = match columns.get("distance").expect("distance column") {
        gleaph_gql_ic::GqlWireValue::Float64(d) => *d,
        other => panic!("distance must be Float64, got {other:?}"),
    };
    assert!(
        (distance - 0.0f64).abs() < 1e-6,
        "exact match distance must be zero"
    );
}

#[test]
fn graph_delete_drains_vector_remove_through_typed_batch() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    ensure_user_graph_type(&env);
    register(&env, vector);
    fully_activate(&env, vector);

    let inserted = e2e_insert_vertex_with_label(&env, env.graph_source, user_vertex_label_id(&env));
    let result = ingest(
        &env,
        encode_vertex(&env, inserted.local_vertex_id),
        vec![6.0; DIMS as usize],
    );
    assert_eq!(
        result.projection_outcome,
        VertexEmbeddingProjectionOutcome::Applied,
        "the typed Router batch path seeds one searchable vector"
    );
    assert_eq!(router_vector_hit_count(&env, 6.0), 1);

    // Preserve the typed wire contract independently of Router ingestion. This stale remove is an
    // idempotent no-op, but the Graph caller must still receive a valid typed progress outcome.
    let stale_noop = VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: inserted.local_vertex_id,
        },
        mutation_id: 0,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        bytes: Vec::new(),
        remove: true,
    };
    let bytes = env
        .pic
        .update_call(
            vector,
            env.graph_source,
            "vector_sync_batch_outcome",
            Encode!(&vec![stale_noop]).expect("encode typed vector batch"),
        )
        .expect("typed vector batch call");
    let outcome: Result<VectorSyncBatchOutcome, VectorSyncBatchUnavailable> =
        Decode!(&bytes, Result<VectorSyncBatchOutcome, VectorSyncBatchUnavailable>)
            .expect("decode typed vector batch outcome");
    assert_eq!(outcome, Ok(VectorSyncBatchOutcome::Progress { applied: 1 }));
    assert_eq!(
        router_vector_hit_count(&env, 6.0),
        1,
        "the stale replay must not remove the current vector"
    );

    // Graph DML queues the derived remove in its durable outbox. The synchronous outbox drain uses
    // the live IcVectorCanisterClient additive `vector_sync_batch_outcome` endpoint and consumes the
    // typed Progress prefix before acknowledging the local outbox entry.
    gql_mutate_as_admin(
        &env,
        "MATCH (v:User) DETACH DELETE v",
        "typed-vector-delete",
    );
    assert_eq!(
        graph_derived_index_outbox_len(&env),
        0,
        "the typed Progress outcome acknowledges the Graph outbox prefix"
    );
    assert_eq!(
        router_vector_hit_count(&env, 6.0),
        0,
        "the Graph-delivered typed remove reaches live Vector state"
    );
}

#[test]
fn unavailable_vector_owner_rebinds_graph_and_router_direct_ingestion_outboxes() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    ensure_user_graph_type(&env);
    register(&env, vector);
    fully_activate(&env, vector);

    let inserted = e2e_insert_vertex_with_label(&env, env.graph_source, user_vertex_label_id(&env));
    let result = ingest(
        &env,
        encode_vertex(&env, inserted.local_vertex_id),
        vec![6.0; DIMS as usize],
    );
    assert_eq!(
        result.projection_outcome,
        VertexEmbeddingProjectionOutcome::Applied,
        "the vector must be searchable before simulating the unavailable owner"
    );
    assert_eq!(router_vector_hit_count(&env, 6.0), 1);

    e2e_unbind_vector_definition_store(&env, vector);

    // The Router guard admits this probe, while the typed endpoint must report the outer
    // availability marker before attempting any row/page/subject mutation.
    let unavailable_probe = VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: inserted.local_vertex_id,
        },
        mutation_id: u64::MAX,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        bytes: Vec::new(),
        remove: true,
    };
    let bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "vector_sync_batch_outcome",
            Encode!(&vec![unavailable_probe]).expect("encode unavailable vector probe"),
        )
        .expect("typed vector probe call");
    let outcome: Result<VectorSyncBatchOutcome, VectorSyncBatchUnavailable> =
        Decode!(&bytes, Result<VectorSyncBatchOutcome, VectorSyncBatchUnavailable>)
            .expect("decode unavailable vector outcome");
    assert_eq!(
        outcome,
        Err(VectorSyncBatchUnavailable::IndexDefinitionStoreUnavailable),
        "an unavailable owner is an outer typed error before any operation commits"
    );

    // Graph DML is still canonical: it commits the delete and durable outbox row, but the
    // unavailable vector leaves the projection pending for maintenance retry.
    gql_mutate_as_admin(
        &env,
        "MATCH (v:User) DETACH DELETE v",
        "typed-vector-delete-owner-unavailable",
    );
    assert_eq!(
        graph_derived_index_outbox_len(&env),
        1,
        "the unavailable vector must leave the Graph delete posting pending"
    );

    // The normal post-upgrade hook performs exact-open on the existing MemoryId 4 bytes. The
    // Graph timer can then retry the untouched outbox row and remove the deleted vector.
    env.pic
        .upgrade_canister(
            vector,
            wasm_bytes("VECTOR_INDEX_WASM"),
            Encode!(&()).expect("encode vector upgrade args"),
            None,
        )
        .expect("upgrade vector canister");
    drain_maintenance_via_timer(&env, env.graph_source);
    assert_eq!(graph_derived_index_outbox_len(&env), 0);
    assert_eq!(router_vector_hit_count(&env, 6.0), 0);

    // Let the ordinary Router recovery timer finish its idle lap before creating direct-ingest
    // work. This keeps the upgrade boundary below about the exact pending row, rather than a
    // concurrently scheduled older recovery pass.
    run_router_recovery_timer(&env);

    let inserted = e2e_insert_vertex_with_label(&env, env.graph_source, user_vertex_label_id(&env));
    let expected_id = vertex_element_id(&env);
    e2e_unbind_vector_definition_store(&env, vector);

    // Graph stamp allocation is canonical even while Vector is unavailable. The returned deferred
    // result identifies the committed version; the exact subject is asserted by the search after
    // the durable Router outbox replays.
    let deferred = ingest(
        &env,
        encode_vertex(&env, inserted.local_vertex_id),
        vec![7.0; DIMS as usize],
    );
    assert!(deferred.embedding_version > 0);
    assert_eq!(
        deferred.projection_outcome,
        VertexEmbeddingProjectionOutcome::Pending,
        "unavailable Vector must defer the direct-ingest projection"
    );

    // Both the Router-owned operation and its exact Vector target must survive the Router upgrade.
    env.pic
        .upgrade_canister(
            env.router,
            wasm_bytes("ROUTER_WASM"),
            Encode!(&()).expect("encode router upgrade args"),
            None,
        )
        .expect("upgrade router with pending direct-ingest outbox");
    env.pic
        .upgrade_canister(
            vector,
            wasm_bytes("VECTOR_INDEX_WASM"),
            Encode!(&()).expect("encode vector upgrade args"),
            None,
        )
        .expect("rebind vector definition store");

    run_router_recovery_timer(&env);

    let query = format!(
        "MATCH (d) SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) DISTANCE AS distance RETURN ELEMENT_ID(d) AS d_id, distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(7.0)),
    )])
    .expect("encode direct-ingest replay params");
    let gql = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        gql.row_count, 1,
        "replayed direct ingestion returns one row"
    );
    let rows_blob = gql.rows_blob.expect("replayed direct-ingest rows blob");
    let wire = GqlWireRows::decode_blob(&rows_blob).expect("decode replayed direct-ingest rows");
    assert_eq!(wire.rows.len(), 1);
    let columns: std::collections::BTreeMap<String, gleaph_gql_ic::GqlWireValue> =
        wire.rows[0].columns.clone().into_iter().collect();
    assert_eq!(
        columns.get("d_id").expect("d_id column"),
        &expected_id,
        "replay must target the exact stamped vertex subject"
    );
    let distance = match columns.get("distance").expect("distance column") {
        gleaph_gql_ic::GqlWireValue::Float64(d) => *d,
        other => panic!("distance must be Float64, got {other:?}"),
    };
    assert!(
        (distance - 0.0f64).abs() < 1e-6,
        "replayed exact match distance must be zero"
    );

    // A later idle lap must not duplicate or corrupt the idempotent vector row.
    run_router_recovery_timer(&env);
    assert_eq!(
        router_vector_hit_count(&env, 7.0),
        1,
        "replaying an already acknowledged direct-ingest row must remain one exact hit"
    );
}

#[test]
fn graph_response_loss_preserves_pregraph_intent_across_router_upgrade() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    ensure_user_graph_type(&env);
    register(&env, vector);
    fully_activate(&env, vector);

    let inserted = e2e_insert_vertex_with_label(&env, env.graph_source, user_vertex_label_id(&env));
    let encoded = encode_vertex(&env, inserted.local_vertex_id);
    set_embedding_stamp_response_loss(&env, true);

    let pending = ingest(&env, encoded, vec![8.0; DIMS as usize]);
    assert_eq!(
        pending.projection_outcome,
        VertexEmbeddingProjectionOutcome::Pending,
        "a lost Graph response must leave Router as the durable non-terminal owner"
    );
    assert_eq!(
        router_vector_hit_count(&env, 8.0),
        0,
        "Vector must not receive an intent before Graph acceptance is observed"
    );

    env.pic
        .upgrade_canister(
            env.router,
            wasm_bytes("ROUTER_WASM"),
            Encode!(&()).expect("encode router upgrade args"),
            None,
        )
        .expect("upgrade Router with AwaitingGraph intent");
    set_embedding_stamp_response_loss(&env, false);
    run_router_recovery_timer(&env);

    assert_eq!(
        router_vector_hit_count(&env, 8.0),
        1,
        "recovery must replay the exact Graph request and Vector bytes after Router upgrade"
    );
    run_router_recovery_timer(&env);
    assert_eq!(
        router_vector_hit_count(&env, 8.0),
        1,
        "a later recovery lap must remain idempotent"
    );
}

#[test]
fn graph_only_markerless_lane_advances_frontier_on_autonomous_timer() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    ensure_user_graph_type(&env);
    register(&env, vector);
    fully_activate(&env, vector);

    let seeded = e2e_insert_vertex_with_label(&env, env.graph_source, user_vertex_label_id(&env));
    let expected_seeded_id = vertex_element_id(&env);

    // The Vector storage owner must reject even the authorized Router when the exact shard is not
    // attached. This probes the fail-closed check below the public caller guard before the real
    // markerless lane lifecycle starts.
    let unattached_before = vector_frontier_probe(&env, vector, seeded.local_vertex_id);
    let unattached_receipt_before = vector_frontier_receipt_probe(&env, vector);
    let unattached_bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "admin_advance_router_frontier",
            Encode!(&ShardId::new(1), &1_u64)
                .expect("encode unattached markerless frontier publication"),
        )
        .expect("unattached markerless frontier publication call");
    let unattached_result: Result<(), String> = Decode!(&unattached_bytes, Result<(), String>)
        .expect("decode unattached markerless frontier publication");
    assert_eq!(
        unattached_result,
        Err("caller is not an attached graph shard".to_string())
    );
    assert_eq!(
        vector_frontier_probe(&env, vector, seeded.local_vertex_id),
        unattached_before,
        "an unattached exact lane must preserve all frontier and subject state"
    );
    assert_eq!(
        vector_frontier_receipt_probe(&env, vector),
        unattached_receipt_before,
        "an unattached exact lane must not record a successful frontier receipt"
    );

    // Allocate m1 with ordinary Graph DML, then seed the chosen subject through the production
    // Graph-owned typed batch boundary. This creates no Router direct-ingestion intent or setup
    // marker in MemoryId 53.
    let setup =
        gql_mutate_result_as_admin(&env, "INSERT (:User)", "markerless-frontier-setup-ceiling");
    let m1 = setup
        .token
        .as_ref()
        .expect("setup Graph mutation token")
        .mutation_id;
    let setup_upsert = VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: seeded.local_vertex_id,
        },
        mutation_id: m1,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        bytes: vec_bytes(6.0),
        remove: false,
    };
    let setup_bytes = env
        .pic
        .update_call(
            vector,
            env.graph_source,
            "vector_sync_batch_outcome",
            Encode!(&vec![setup_upsert]).expect("encode Graph-owned setup batch"),
        )
        .expect("Graph-owned setup batch call");
    let setup_outcome: Result<VectorSyncBatchOutcome, VectorSyncBatchUnavailable> = Decode!(
        &setup_bytes,
        Result<VectorSyncBatchOutcome, VectorSyncBatchUnavailable>
    )
    .expect("decode Graph-owned setup batch");
    assert_eq!(
        setup_outcome,
        Ok(VectorSyncBatchOutcome::Progress { applied: 1 })
    );
    assert_eq!(
        router_vector_ingest_probe(&env),
        (m1, Vec::new()),
        "Graph-owned setup must leave the Router direct-ingestion outbox empty"
    );
    assert_eq!(
        vector_frontier_probe(&env, vector, seeded.local_vertex_id),
        (m1, 0, Some((m1, false)), false, None),
        "Graph-owned setup must advance only the Graph watermark and retain the live subject"
    );
    assert_eq!(
        router_vector_search_subjects(&env, 6.0),
        vec![expected_seeded_id.clone()],
        "the bounded Graph-owned setup must create the exact live subject"
    );

    // The Graph-only delete raises the Router allocation ceiling without creating a direct-ingest
    // row. Until the ordinary Router timer publishes the markerless frontier, the exact deleted
    // clock and deleted-list entry remain as the replay fence.
    let delete = gql_mutate_result_as_admin(
        &env,
        "MATCH (v:User) DETACH DELETE v",
        "markerless-frontier-graph-delete",
    );
    let m2 = delete
        .token
        .as_ref()
        .expect("Graph delete mutation token")
        .mutation_id;
    assert!(m2 > m1, "Graph delete must raise the mutation ceiling");
    assert_eq!(
        router_vector_ingest_probe(&env),
        (m2, Vec::new()),
        "Graph-only delete must not synthesize a Router MemoryId 53 marker"
    );
    assert_eq!(
        vector_frontier_probe(&env, vector, seeded.local_vertex_id),
        (m2, 0, Some((m2, true)), true, None),
        "the Graph watermark must lead while the Router frontier still gates physical GC"
    );
    assert!(
        router_vector_search_subjects(&env, 6.0).is_empty(),
        "the Graph tombstone must hide the exact subject before physical collection"
    );

    // Lose the successful frontier response after Vector commits its monotonic watermark and GC.
    // No direct recovery ingress is used: simulated time and ordinary ticks fire the Router timer.
    let receipt_before_loss = vector_frontier_receipt_probe(&env, vector).0;
    arm_router_fault(&env, FRONTIER_REPLY_AFTER_COMMIT_FAULT);
    run_router_recovery_timer(&env);
    assert_eq!(
        vector_frontier_probe(&env, vector, seeded.local_vertex_id),
        (m2, m2, None, false, None),
        "the markerless timer must advance the Router watermark and physically collect the exact tombstone"
    );
    assert_eq!(
        vector_frontier_receipt_probe(&env, vector),
        (receipt_before_loss + 1, Some((ShardId::new(0).raw(), m2))),
        "the lost response must follow one committed markerless frontier call"
    );
    assert_eq!(
        router_vector_ingest_probe(&env),
        (m2, Vec::new()),
        "markerless response loss must not create a durable Router retry row"
    );
    assert!(
        router_vector_search_subjects(&env, 6.0).is_empty(),
        "physical collection must not resurrect the exact deleted subject"
    );

    // Router and Vector upgrades reset only heap scheduling/receipt state. Stable catalog
    // rediscovery retries the same safe ceiling through the ordinary post-upgrade timer.
    env.pic
        .upgrade_canister(
            env.router,
            wasm_bytes("ROUTER_WASM"),
            Encode!(&()).expect("encode Router upgrade args"),
            None,
        )
        .expect("upgrade Router after markerless response loss");
    env.pic
        .upgrade_canister(
            vector,
            wasm_bytes("VECTOR_INDEX_WASM"),
            Encode!(&()).expect("encode Vector upgrade args"),
            None,
        )
        .expect("upgrade Vector after markerless response loss");
    assert_eq!(
        vector_frontier_probe(&env, vector, seeded.local_vertex_id),
        (m2, m2, None, false, None),
        "Vector upgrade must preserve the watermark and physical collection"
    );
    assert_eq!(
        vector_frontier_receipt_probe(&env, vector),
        (0, None),
        "Vector upgrade must reset only the heap-only receipt"
    );
    assert_eq!(router_vector_ingest_probe(&env), (m2, Vec::new()));

    run_router_recovery_timer(&env);
    assert_eq!(
        vector_frontier_receipt_probe(&env, vector),
        (1, Some((ShardId::new(0).raw(), m2))),
        "post-upgrade catalog rediscovery must retry the markerless frontier autonomously"
    );
    assert_eq!(
        vector_frontier_probe(&env, vector, seeded.local_vertex_id),
        (m2, m2, None, false, None),
        "the idempotent retry must preserve physical GC and the monotonic watermark"
    );
    assert_eq!(router_vector_ingest_probe(&env), (m2, Vec::new()));
    assert!(
        router_vector_search_subjects(&env, 6.0).is_empty(),
        "the exact deleted subject must not resurrect after restart and retry"
    );
}

#[test]
fn contiguous_router_frontier_survives_response_loss_upgrade_and_gates_gc() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    ensure_user_graph_type(&env);
    register(&env, vector);
    fully_activate(&env, vector);

    // The Router-only publication endpoint must reject a direct admin caller without changing the
    // exact Vector state that the later frontier lifecycle will use.
    let unauthorized_before = vector_frontier_probe(&env, vector, 0);
    let unauthorized = env.pic.update_call(
        vector,
        env.admin,
        "admin_advance_router_frontier",
        Encode!(&ShardId::new(0), &1_u64).expect("encode unauthorized frontier publication"),
    );
    assert!(
        unauthorized.is_err(),
        "a direct admin caller must be rejected by the Router-only endpoint"
    );
    assert_eq!(
        vector_frontier_probe(&env, vector, 0),
        unauthorized_before,
        "an unauthorized frontier call must not mutate any Vector probe field"
    );
    assert_eq!(
        router_vector_ingest_probe(&env),
        (0, Vec::new()),
        "the unauthorized Vector call must not allocate or create a Router outbox row"
    );
    assert_eq!(
        vector_frontier_receipt_probe(&env, vector),
        (0, None),
        "the rejected frontier call must not create a success receipt"
    );

    // The authorized Router must still be rejected below the public guard when it names an
    // unattached shard. The store-level error must be returned without mutating Vector storage or
    // the heap-only success receipt.
    let detached_before = vector_frontier_probe(&env, vector, 0);
    let detached_bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "admin_advance_router_frontier",
            Encode!(&ShardId::new(1), &1_u64)
                .expect("encode unattached-shard frontier publication"),
        )
        .expect("unattached-shard frontier publication call");
    let detached_result: Result<(), String> = Decode!(&detached_bytes, Result<(), String>)
        .expect("decode unattached-shard frontier publication");
    assert_eq!(
        detached_result,
        Err("caller is not an attached graph shard".to_string()),
        "the authorized Router must receive the store-level unattached-shard error"
    );
    assert_eq!(
        vector_frontier_probe(&env, vector, 0),
        detached_before,
        "an unattached-shard frontier call must not mutate any Vector storage probe field"
    );
    assert_eq!(
        vector_frontier_receipt_probe(&env, vector),
        (0, None),
        "a failed store call must not create a heap success receipt"
    );

    // m10: Vector commits the live subject, then the Router loses the successful reply before its
    // exact row can move from AwaitingVector to AwaitingFrontier.
    let first = e2e_insert_vertex_with_label(&env, env.graph_source, user_vertex_label_id(&env));
    let expected_first_id = vertex_element_id(&env);
    arm_router_fault(&env, VECTOR_SYNC_REPLY_AFTER_COMMIT_FAULT);
    let m10_result = ingest(
        &env,
        encode_vertex(&env, first.local_vertex_id),
        vec![6.0; DIMS as usize],
    );
    assert_eq!(
        m10_result.projection_outcome,
        VertexEmbeddingProjectionOutcome::Pending,
        "a post-commit Vector reply loss must remain Pending at Router ingress"
    );
    assert_eq!(
        vector_frontier_probe(&env, vector, first.local_vertex_id),
        (
            0,
            0,
            Some((m10_result.embedding_version, false)),
            false,
            None
        ),
        "Vector must durably retain the live m10 subject while Router has not applied the reply"
    );
    assert_eq!(
        router_vector_search_subjects(&env, 6.0),
        vec![expected_first_id.clone()],
        "the post-commit Vector subject must be searchable before Router applies the reply"
    );
    assert_eq!(
        router_vector_ingest_probe(&env),
        (
            m10_result.embedding_version,
            vec![(m10_result.embedding_version, vector, ShardId::new(0), 1,)]
        ),
        "the exact m10 row must remain AwaitingVector after the response loss"
    );
    arm_router_fault(&env, 0);

    // m11: Graph removes the same subject. The Graph-owned watermark and Vector tombstone advance,
    // but Router's watermark remains zero, so the deleted-list fence cannot be collected yet.
    let delete_result = gql_mutate_result_as_admin(
        &env,
        "MATCH (v:User) DETACH DELETE v",
        "contiguous-frontier-delete",
    );
    let m11 = delete_result
        .token
        .as_ref()
        .expect("Graph delete mutation token")
        .mutation_id;
    assert!(m11 > m10_result.embedding_version, "m11 must follow m10");
    assert_eq!(
        vector_frontier_probe(&env, vector, first.local_vertex_id),
        (m11, 0, Some((m11, true)), true, None,),
        "Graph deletion must retain the exact m11 tombstone and deleted-list fence"
    );
    assert_eq!(
        router_vector_ingest_probe(&env),
        (
            m11,
            vec![(m10_result.embedding_version, vector, ShardId::new(0), 1,)]
        ),
        "Graph delete must advance the Router allocation ceiling for m11 without allocating a Router marker"
    );
    assert!(
        router_vector_search_subjects(&env, 6.0).is_empty(),
        "the Graph tombstone must hide the deleted subject before Router frontier publication"
    );

    // m12: a higher unrelated Router direct-ingest marker is committed to Vector. It must not
    // allow a frontier to pass unresolved m10, and its own subject identity must be preserved.
    let higher = e2e_insert_vertex_with_label(&env, env.graph_source, user_vertex_label_id(&env));
    let expected_higher_id = vertex_element_id(&env);
    let m12_result = ingest(
        &env,
        encode_vertex(&env, higher.local_vertex_id),
        vec![9.0; DIMS as usize],
    );
    assert_eq!(
        m12_result.projection_outcome,
        VertexEmbeddingProjectionOutcome::Applied,
        "the unrelated higher direct-ingest marker must commit independently"
    );
    assert_eq!(
        m12_result.embedding_version,
        m11 + 1,
        "m12 must be the higher mutation immediately after the Graph delete"
    );
    assert_ne!(
        expected_higher_id, expected_first_id,
        "the higher direct-ingest subject must be distinct from the deleted subject"
    );
    assert_eq!(
        vector_frontier_probe(&env, vector, higher.local_vertex_id),
        (
            m11,
            0,
            Some((m12_result.embedding_version, false)),
            false,
            None
        ),
        "the higher direct-ingest subject must remain live without a Router frontier"
    );
    assert_eq!(
        router_vector_ingest_probe(&env),
        (
            m12_result.embedding_version,
            vec![
                (m10_result.embedding_version, vector, ShardId::new(0), 1,),
                (m12_result.embedding_version, vector, ShardId::new(0), 2),
            ]
        ),
        "m10 must stay AwaitingVector while only the unrelated m12 marker is AwaitingFrontier"
    );

    // Attempt recovery while m10 is unresolved. The post-commit batch fault keeps the exact m10
    // row unresolved, so no safe snapshot contains m12 and Vector GC remains gated.
    arm_router_fault(&env, VECTOR_SYNC_REPLY_AFTER_COMMIT_FAULT);
    run_router_recovery_timer(&env);
    assert_eq!(
        router_vector_search_subjects(&env, 6.0),
        vec![expected_higher_id.clone()],
        "only the live m12 subject must remain searchable; deleted m10 must remain absent while its Router marker is unresolved"
    );
    assert!(
        router_vector_search_subjects(&env, 9.0) == vec![expected_higher_id.clone()],
        "the higher unrelated subject must remain searchable while m10 blocks the frontier"
    );
    assert_eq!(
        vector_frontier_probe(&env, vector, first.local_vertex_id),
        (m11, 0, Some((m11, true)), true, None),
        "the unresolved m10 marker must keep the tombstone and deleted-list entry retained"
    );
    assert_eq!(
        router_vector_ingest_probe(&env),
        (
            m12_result.embedding_version,
            vec![
                (m10_result.embedding_version, vector, ShardId::new(0), 1,),
                (m12_result.embedding_version, vector, ShardId::new(0), 2),
            ]
        ),
        "blocked recovery must preserve both exact marker phases"
    );
    arm_router_fault(&env, 0);

    // Resolve m10 through the durable Router retry while arming the frontier response-loss fault.
    // Vector must ignore/no-op the stale m10 upsert against the newer m11 tombstone, then durably apply
    // the safe m12 frontier and GC before Router can retire either marker.
    arm_router_fault(&env, FRONTIER_REPLY_AFTER_COMMIT_FAULT);
    run_router_recovery_timer(&env);
    assert_eq!(
        vector_frontier_probe(&env, vector, first.local_vertex_id),
        (m11, m12_result.embedding_version, None, false, None),
        "Vector must durably advance the router watermark and GC m11 before the Router reply"
    );
    assert_eq!(
        vector_frontier_receipt_probe(&env, vector),
        (
            1,
            Some((ShardId::new(0).raw(), m12_result.embedding_version))
        ),
        "the first lost frontier reply must follow exactly one successful Vector store call for shard 0 at m12"
    );
    assert_eq!(
        router_vector_ingest_probe(&env),
        (
            m12_result.embedding_version,
            vec![
                (m10_result.embedding_version, vector, ShardId::new(0), 2,),
                (m12_result.embedding_version, vector, ShardId::new(0), 2),
            ]
        ),
        "lost frontier response must retain the exact m10/m12 marker snapshot"
    );

    // Both canisters must preserve their exact durable ownership across upgrades before the
    // observed frontier retry is allowed to retire Router markers.
    env.pic
        .upgrade_canister(
            env.router,
            wasm_bytes("ROUTER_WASM"),
            Encode!(&()).expect("encode Router upgrade args"),
            None,
        )
        .expect("upgrade Router with retained frontier markers");
    assert_eq!(
        router_vector_ingest_probe(&env),
        (
            m12_result.embedding_version,
            vec![
                (m10_result.embedding_version, vector, ShardId::new(0), 2,),
                (m12_result.embedding_version, vector, ShardId::new(0), 2),
            ]
        ),
        "Router upgrade must preserve the exact frontier marker ownership"
    );
    env.pic
        .upgrade_canister(
            vector,
            wasm_bytes("VECTOR_INDEX_WASM"),
            Encode!(&()).expect("encode Vector upgrade args"),
            None,
        )
        .expect("upgrade Vector with retained tombstone-GC state");
    assert_eq!(
        vector_frontier_probe(&env, vector, first.local_vertex_id),
        (m11, m12_result.embedding_version, None, false, None),
        "Vector upgrade must preserve watermarks and the physically collected tombstone"
    );
    assert_eq!(
        vector_frontier_receipt_probe(&env, vector),
        (0, None),
        "Vector upgrade must reset the heap-only frontier receipt"
    );
    assert_eq!(
        vector_frontier_probe(&env, vector, higher.local_vertex_id),
        (
            m11,
            m12_result.embedding_version,
            Some((m12_result.embedding_version, false)),
            false,
            None
        ),
        "Vector upgrade must preserve the higher live subject identity"
    );
    arm_router_fault(&env, 0);

    // The observed frontier retry is idempotent and retires exactly the captured marker snapshot.
    run_router_recovery_timer(&env);
    assert_eq!(
        vector_frontier_receipt_probe(&env, vector),
        (
            1,
            Some((ShardId::new(0).raw(), m12_result.embedding_version))
        ),
        "the observed post-upgrade retry must advance the reset receipt 0 to 1 for shard 0 at m12"
    );
    assert_eq!(
        router_vector_ingest_probe(&env),
        (m12_result.embedding_version, Vec::new()),
        "the observed frontier reply must retire exactly m10 and m12"
    );
    assert_eq!(
        vector_frontier_probe(&env, vector, first.local_vertex_id),
        (m11, m12_result.embedding_version, None, false, None),
        "retiring Router markers must not recreate the collected m10 tombstone"
    );
    assert_eq!(
        vector_frontier_probe(&env, vector, higher.local_vertex_id),
        (
            m11,
            m12_result.embedding_version,
            Some((m12_result.embedding_version, false)),
            false,
            None
        ),
        "retiring Router markers must preserve the higher live subject clock"
    );

    // A later idle lap is a no-op: no marker is recreated, no counter advances, and stale m10 never
    // resurrects while the higher unrelated subject remains searchable.
    let idle_before = router_vector_ingest_probe(&env);
    run_router_recovery_timer(&env);
    assert_eq!(router_vector_ingest_probe(&env), idle_before);
    assert_eq!(
        router_vector_search_subjects(&env, 6.0),
        vec![expected_higher_id.clone()],
        "a later recovery lap must keep the live m12 subject as the sole hit and never resurrect stale m10"
    );
    assert_eq!(
        router_vector_search_subjects(&env, 9.0),
        vec![expected_higher_id]
    );
}

#[test]
fn invalid_ingestion_fails_closed_before_graph_call() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    ensure_user_graph_type(&env);
    register(&env, vector);
    fully_activate(&env, vector);

    let inserted = e2e_insert_vertex_with_label(&env, env.graph_source, user_vertex_label_id(&env));

    // Dimension mismatch: values length 2 vs registered dims 16.
    let args = AdminIngestVertexEmbeddingBatchArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        embedding_name: EMBEDDING_NAME.to_string(),
        items: vec![AdminIngestVertexEmbeddingBatchItem {
            encoded_vertex_id: encode_vertex(&env, inserted.local_vertex_id),
            values: vec![1.0, 2.0],
        }],
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "ingest_vertex_embeddings",
            Encode!(&args).expect("encode"),
        )
        .expect("ingest call");
    let result: Result<
        Vec<Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, String>>,
        gleaph_graph_kernel::federation::RouterError,
    > = Decode!(
        &bytes,
        Result<
            Vec<Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, String>>,
            gleaph_graph_kernel::federation::RouterError,
        >
    )
    .expect("decode");
    assert!(
        matches!(
            result,
            Err(gleaph_graph_kernel::federation::RouterError::InvalidArgument(_))
        ),
        "dimension mismatch must fail closed: {result:?}"
    );

    // A subsequent valid ingestion on the same vertex must still succeed, proving no partial state.
    let valid = ingest(
        &env,
        encode_vertex(&env, inserted.local_vertex_id),
        vec![6.0; DIMS as usize],
    );
    assert_eq!(valid.embedding_version, 1);
}

/// I8 end-to-end: an I8-registered index accepts an F32 wire embedding through Router -> Graph stamp
/// -> vector canister, quantizes at write, and returns the ingested vertex as the exact match.
#[test]
fn i8_canonical_ingestion_reaches_router_vector_search() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    // The vector registration labels must already exist in the Router's vertex-label catalog, which
    // is populated by graph-type DDL (the federation fixture creates no graph type).
    ensure_user_graph_type(&env);
    register_with_encoding(&env, vector, Some(VectorEncoding::I8));
    fully_activate(&env, vector);

    let inserted = e2e_insert_vertex_with_label(&env, env.graph_source, user_vertex_label_id(&env));
    let encoded = encode_vertex(&env, inserted.local_vertex_id);

    let result = ingest(&env, encoded, vec![6.0; DIMS as usize]);
    assert!(
        matches!(
            result.projection_outcome,
            VertexEmbeddingProjectionOutcome::Applied
        ),
        "I8 projection should be applied on an activated index"
    );

    // Router validates the query as canonical F32 (`dims*4`) and forwards `encoding = I8`; the
    // vector canister selects the fused i8×f32 kernels. A constant vector quantizes losslessly, so
    // the exact GQL search returns the ingested vertex at (near-)zero distance.
    let expected_id = vertex_element_id(&env);
    let query = format!(
        "MATCH (d) SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) DISTANCE AS distance RETURN ELEMENT_ID(d) AS d_id, distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(6.0)),
    )])
    .expect("encode params");
    let gql = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(gql.row_count, 1, "I8 exact GQL search returns one row");
    let rows_blob = gql.rows_blob.expect("rows blob");
    let wire = GqlWireRows::decode_blob(&rows_blob).expect("decode rows");
    assert_eq!(wire.rows.len(), 1);
    let columns: std::collections::BTreeMap<String, gleaph_gql_ic::GqlWireValue> =
        wire.rows[0].columns.clone().into_iter().collect();
    assert_eq!(
        columns.get("d_id").expect("d_id column"),
        &expected_id,
        "I8 GQL must return the ingested vertex ELEMENT_ID"
    );
    let distance = match columns.get("distance").expect("distance column") {
        gleaph_gql_ic::GqlWireValue::Float64(d) => *d,
        other => panic!("distance must be Float64, got {other:?}"),
    };
    assert!(
        (distance - 0.0f64).abs() < 1e-3,
        "I8 exact match distance must be ~zero, got {distance}"
    );
}
