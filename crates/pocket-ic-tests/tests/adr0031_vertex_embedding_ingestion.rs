//! PocketIC contract for plan 0048 canonical vertex-embedding ingestion through Router.
//!
//! Proves that an authorized caller can write one finite F32 embedding via Router using only the
//! graph name, opaque encoded vertex id, embedding name, and values; that Graph owns the canonical
//! bytes/version; that the derived vector-index converges without direct vector-canister seeding;
//! and that invalid inputs fail closed before any Graph call.

use candid::{Decode, Encode, Principal};
use gleaph_gql::Value;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::federation::{
    ElementIdEncodingKey, GlobalVertexId, ShardId, encode_global_vertex_id,
};
use gleaph_graph_kernel::vector_index::{
    VectorMetric, VectorSearchResult, VectorSubject, VertexEmbeddingProjectionOutcome,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, e2e_insert_vertex, gql_query_with_params_as_admin,
    install_single_shard_federation, install_vector_canister,
};
use gleaph_router::types::{
    AdminAttachVectorIndexShardArgs, AdminIngestVertexEmbeddingArgs, RegisterVectorIndexArgs,
    RouterVectorSearchRequest,
};

const EMBEDDING_NAME: &str = "ingest_title_vec";
const INDEX_ID: u32 = 1;
const DIMS: u16 = 16;

fn register(env: &FederationEnv, vector: Principal) {
    let args = RegisterVectorIndexArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        embedding_name: EMBEDDING_NAME.to_string(),
        index_id: INDEX_ID,
        dims: DIMS,
        metric: Some(VectorMetric::L2Squared),
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

fn set_dispatch_activation(env: &FederationEnv, enabled: bool) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_set_vector_dispatch_activation",
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
            "admin_set_vector_index_canister",
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
        vector_index_canister: vector,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_attach_vector_index_shard",
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
                "lookup_graph_id",
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
            "graph_element_id_encoding_key",
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
    let args = AdminIngestVertexEmbeddingArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        encoded_vertex_id: encoded,
        embedding_name: EMBEDDING_NAME.to_string(),
        values,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_ingest_vertex_embedding",
            Encode!(&args).expect("encode"),
        )
        .expect("ingest call");
    let result: Result<
        gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult,
        gleaph_graph_kernel::federation::RouterError,
    > = Decode!(
        &bytes,
        Result<
            gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult,
            gleaph_graph_kernel::federation::RouterError,
        >
    )
    .expect("decode");
    result.expect("ingest succeeds")
}

fn router_vector_search(env: &FederationEnv, value: f32, top_k: u32) -> VectorSearchResult {
    let req = RouterVectorSearchRequest {
        logical_graph_name: GRAPH_NAME.to_string(),
        index_id: INDEX_ID,
        query: vec_bytes(value),
        dims: DIMS,
        top_k,
    };
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "vector_search",
            Encode!(&req).expect("encode"),
        )
        .expect("vector search call");
    let result: Result<VectorSearchResult, gleaph_graph_kernel::federation::RouterError> =
        Decode!(&bytes, Result<VectorSearchResult, gleaph_graph_kernel::federation::RouterError>)
            .expect("decode");
    result.expect("search succeeds")
}

fn vertex_element_id(env: &FederationEnv) -> gleaph_gql_ic::IcWireValue {
    let result =
        gql_query_with_params_as_admin(env, "MATCH (v) RETURN ELEMENT_ID(v) AS v_id", vec![]);
    assert_eq!(result.row_count, 1, "exactly one vertex");
    let rows_blob = result.rows_blob.expect("rows blob");
    let wire = IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode rows");
    assert_eq!(wire.rows.len(), 1);
    let mut columns: std::collections::BTreeMap<String, gleaph_gql_ic::IcWireValue> = wire
        .rows
        .into_iter()
        .next()
        .expect("one row")
        .columns
        .into_iter()
        .collect();
    columns.remove("v_id").expect("v_id column")
}

#[test]
fn canonical_ingestion_reaches_router_vector_search_without_direct_seeding() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    register(&env, vector);
    fully_activate(&env, vector);

    let inserted = e2e_insert_vertex(&env, env.graph_source);
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

    let search = router_vector_search(&env, 6.0, 10);
    assert!(
        !search.hits.is_empty(),
        "ingested embedding must be searchable"
    );
    let nearest = &search.hits[0];
    assert_eq!(
        nearest.subject,
        VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: inserted.local_vertex_id,
        },
        "nearest subject must be the ingested vertex"
    );
    assert_eq!(nearest.distance, 0.0, "exact query has zero distance");
    assert_eq!(nearest.embedding_version, 1);

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
    let wire = IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode rows");
    assert_eq!(wire.rows.len(), 1);
    let columns: std::collections::BTreeMap<String, gleaph_gql_ic::IcWireValue> =
        wire.rows[0].columns.clone().into_iter().collect();
    assert_eq!(
        columns.get("d_id").expect("d_id column"),
        &expected_id,
        "GQL must return the ingested vertex ELEMENT_ID"
    );
    let distance = match columns.get("distance").expect("distance column") {
        gleaph_gql_ic::IcWireValue::Float64(d) => *d,
        other => panic!("distance must be Float64, got {other:?}"),
    };
    assert!(
        (distance - 0.0f64).abs() < 1e-6,
        "exact match distance must be zero"
    );
}

#[test]
fn invalid_ingestion_fails_closed_before_graph_call() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    register(&env, vector);
    fully_activate(&env, vector);

    let inserted = e2e_insert_vertex(&env, env.graph_source);

    // Dimension mismatch: values length 2 vs registered dims 16.
    let args = AdminIngestVertexEmbeddingArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        encoded_vertex_id: encode_vertex(&env, inserted.local_vertex_id),
        embedding_name: EMBEDDING_NAME.to_string(),
        values: vec![1.0, 2.0],
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_ingest_vertex_embedding",
            Encode!(&args).expect("encode"),
        )
        .expect("ingest call");
    let result: Result<
        gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult,
        gleaph_graph_kernel::federation::RouterError,
    > = Decode!(
        &bytes,
        Result<
            gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult,
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
