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
    FederationEnv, GRAPH_NAME, drain_maintenance_via_timer, e2e_insert_vertex_with_label,
    ensure_user_graph_type, gql_mutate_as_admin, gql_query_with_params_as_admin,
    install_single_shard_federation, install_vector_canister, run_router_recovery_timer,
    user_vertex_label_id, wasm_bytes,
};
use gleaph_router::types::{
    AdminAttachVectorIndexShardArgs, AdminIngestVertexEmbeddingBatchArgs,
    AdminIngestVertexEmbeddingBatchItem, RegisterVectorIndexArgs, VectorIndexActivationStatus,
};

const EMBEDDING_NAME: &str = "ingest_title_vec";
const INDEX_ID: u32 = 1;
const DIMS: u16 = 16;

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
        VertexEmbeddingProjectionOutcome::DeferredForRepair,
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
