//! PocketIC coverage for ADR 0034 Slice 6: leading `SEARCH ... WHERE` equality filter.
//!
//! Semantics under test:
//!   C = property_index_equal(Document, cat_id, value)
//!   result = vector_top_k(document_embedding, subjects = C, limit)
//!
//! - The filter is resolved through the label-scoped Property Index before vector ranking.
//! - A globally nearer vertex outside the requested category is excluded.

use candid::{Decode, Encode, Principal};
use gleaph_gql::Value;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{RouterError, ShardId};
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorEncoding, VectorMetric, VectorSubject,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, admin_intern_property, admin_intern_vertex_label,
    create_vertex_property_index, e2e_insert_vertex_with_label_and_property,
    gql_query_with_params_as_admin, install_federation, install_vector_canister,
};
use gleaph_router::types::{AdminAttachVectorIndexShardArgs, RegisterVectorIndexArgs};

const EMBEDDING_NAME: &str = "adr0034_doc_vec";
const INDEX_ID: u32 = 1;
const DIMS: u16 = 16;

fn vec_bytes(value: f32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(DIMS as usize * 4);
    for _ in 0..DIMS {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn register_vector_index(env: &FederationEnv, metric: VectorMetric, target: Principal) {
    let args = RegisterVectorIndexArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        embedding_name: EMBEDDING_NAME.to_string(),
        index_id: INDEX_ID,
        dims: DIMS,
        metric: Some(metric),
        target: Some(target),
        if_not_exists: false,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_register_vector_index",
            Encode!(&args).expect("encode register"),
        )
        .expect("admin_register_vector_index call");
    let _: bool = Decode!(&bytes, Result<bool, RouterError>)
        .expect("decode register result")
        .expect("register vector index");
}

fn set_dispatch_activation(env: &FederationEnv, enabled: bool) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_set_vector_dispatch_activation",
            Encode!(&enabled).expect("encode activation"),
        )
        .expect("admin_set_vector_dispatch_activation call");
    let _: () = Decode!(&bytes, Result<(), RouterError>)
        .expect("decode activation result")
        .expect("set activation");
}

fn set_graph_vector_routing(env: &FederationEnv, graph: Principal, vector: Principal) {
    let bytes = env
        .pic
        .update_call(
            graph,
            env.router,
            "admin_set_vector_index_canister",
            Encode!(&vector).expect("encode set vector routing"),
        )
        .expect("admin_set_vector_index_canister call");
    let _: () = Decode!(&bytes, Result<(), String>)
        .expect("decode set vector routing")
        .expect("graph accepts vector routing");
}

fn attach_shard_to_vector(
    env: &FederationEnv,
    vector: Principal,
    graph_id: GraphId,
    shard_id: ShardId,
    shard_canister: Principal,
) {
    let bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "admin_attach_shard_canister",
            Encode!(&graph_id, &shard_id, &shard_canister).expect("encode vector attach"),
        )
        .expect("vector admin_attach_shard_canister call");
    let _: () = Decode!(&bytes, Result<(), String>)
        .expect("decode vector attach")
        .expect("vector accepts shard");
}

fn attach_shard(env: &FederationEnv, shard_id: ShardId, vector: Principal) {
    let args = AdminAttachVectorIndexShardArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        shard_id,
        vector_index_canister: vector,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_attach_vector_index_shard",
            Encode!(&args).expect("encode attach"),
        )
        .expect("admin_attach_vector_index_shard call");
    let _: () = Decode!(&bytes, Result<(), RouterError>)
        .expect("decode attach result")
        .expect("attach shard");
}

fn router_graph_id(env: &FederationEnv) -> GraphId {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "lookup_graph_id",
            Encode!(&GRAPH_NAME.to_string()).expect("encode lookup"),
        )
        .expect("lookup_graph_id call");
    Decode!(&bytes, Result<GraphId, RouterError>)
        .expect("decode lookup_graph_id")
        .expect("graph id")
}

fn enable_vector_dispatch(env: &FederationEnv, vector: Principal) {
    let graph_id = router_graph_id(env);
    set_dispatch_activation(env, true);
    set_graph_vector_routing(env, env.graph_source, vector);
    set_graph_vector_routing(env, env.graph_dest, vector);
    attach_shard_to_vector(env, vector, graph_id, ShardId::new(0), env.graph_source);
    attach_shard_to_vector(env, vector, graph_id, ShardId::new(1), env.graph_dest);
    attach_shard(env, ShardId::new(0), vector);
    attach_shard(env, ShardId::new(1), vector);
}

fn seed_embedding(
    env: &FederationEnv,
    vector: Principal,
    shard_canister: Principal,
    vertex_id: u32,
    value: f32,
) {
    let op = VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id,
        },
        embedding_incarnation: 1,
        embedding_version: 1,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        bytes: vec_bytes(value),
        remove: false,
    };
    let bytes = env
        .pic
        .update_call(
            vector,
            shard_canister,
            "vector_upsert",
            Encode!(&op).expect("encode upsert"),
        )
        .expect("vector_upsert call");
    let _: () = Decode!(
        &bytes,
        Result<(), gleaph_graph_kernel::vector_index::VectorIndexError>
    )
    .expect("decode upsert result")
    .expect("seed embedding");
}

fn extract_rows(
    result: GqlQueryResult,
) -> Vec<std::collections::BTreeMap<String, gleaph_gql_ic::IcWireValue>> {
    let rows_blob = result.rows_blob.expect("rows blob");
    let wire = IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode rows");
    wire.rows
        .into_iter()
        .map(|row| row.columns.into_iter().collect())
        .collect()
}

fn gql_query_with_params_as_admin_result(
    env: &FederationEnv,
    query: &str,
    params: Vec<u8>,
) -> Result<GqlQueryResult, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "gql_query",
            Encode!(&query.to_string(), &params).expect("encode gql_query"),
        )
        .expect("gql_query call");
    Decode!(&bytes,
        Result<GqlQueryResult, RouterError>
    )
    .expect("decode gql_query result")
}

fn setup_search_where_env(env: &FederationEnv, vector: Principal) {
    register_vector_index(env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(env, vector);

    let doc_label_id = admin_intern_vertex_label(env, "Document");
    let cat_id = admin_intern_property(env, "cat_id");
    create_vertex_property_index(
        env,
        "document_cat_id_idx",
        "Document",
        "cat_id",
        "create_document_cat_id_idx",
    );

    // Insert three documents:
    // - vA1: category 1, vector value 1.0 (distance 1 from query 0.0)
    // - vB:  category 2, vector value 0.0 (distance 0 from query 0.0) — globally nearest
    // - vA2: category 1, vector value 2.0 (distance 2 from query 0.0)
    let v_a1 = e2e_insert_vertex_with_label_and_property(
        env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        1,
    );
    let v_b = e2e_insert_vertex_with_label_and_property(
        env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        2,
    );
    let v_a2 = e2e_insert_vertex_with_label_and_property(
        env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        1,
    );

    seed_embedding(env, vector, env.graph_source, v_a1.local_vertex_id, 1.0);
    seed_embedding(env, vector, env.graph_source, v_b.local_vertex_id, 0.0);
    seed_embedding(env, vector, env.graph_source, v_a2.local_vertex_id, 2.0);
}

#[test]
fn search_where_equality_excludes_globally_nearer_vertex() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_env(&env, vector);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 1 \
           LIMIT 10 \
         ) DISTANCE AS distance \
         RETURN ELEMENT_ID(d), distance \
         ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(0.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(result.row_count, 2, "expected two category-1 results");

    let rows = extract_rows(result);
    assert_eq!(rows.len(), 2);
    for row in &rows {
        match row.get("distance").expect("distance column") {
            gleaph_gql_ic::IcWireValue::Float64(d) => {
                assert!(d.is_finite(), "distance must be finite, got {d}");
            }
            other => panic!("distance must be Float64, got {other:?}"),
        }
    }
}

#[test]
fn search_where_equality_parameter_predicate() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_env(&env, vector);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = $cat \
           LIMIT 10 \
         ) DISTANCE AS distance \
         RETURN ELEMENT_ID(d), distance \
         ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![
        ("query".to_string(), Value::Bytes(vec_bytes(0.0))),
        ("cat".to_string(), Value::Int64(1)),
    ])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        result.row_count, 2,
        "expected two category-1 results for parameter predicate"
    );

    let rows = extract_rows(result);
    assert_eq!(rows.len(), 2);
}

#[test]
fn search_where_equality_empty_result_aggregate() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_env(&env, vector);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 999 \
           LIMIT 10 \
         ) DISTANCE AS distance \
         RETURN count(*) AS n"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(0.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        result.row_count, 1,
        "empty candidate set must still produce one aggregate row"
    );

    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    match rows[0].get("n").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(v) => assert_eq!(*v, 0, "count must be zero"),
        other => panic!("count must be Int64 0, got {other:?}"),
    }
}

#[test]
fn search_where_equality_rejects_missing_exact_index() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document");
    let cat_id = admin_intern_property(&env, "cat_id");

    // No property index is created for Document.cat_id.
    let v = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        1,
    );
    seed_embedding(&env, vector, env.graph_source, v.local_vertex_id, 1.0);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 1 \
           LIMIT 10 \
         ) DISTANCE AS distance \
         RETURN ELEMENT_ID(d), distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(0.0)),
    )])
    .expect("encode params");

    let err = gql_query_with_params_as_admin_result(&env, &query, params)
        .expect_err("missing exact index must fail");
    assert!(
        err.to_string().contains("vertex equality index"),
        "missing exact index must fail with a coverage error, got {err}"
    );
}

#[test]
fn search_where_equality_rejects_wrong_label_index() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document");
    let _other_label_id = admin_intern_vertex_label(&env, "Other");
    let cat_id = admin_intern_property(&env, "cat_id");

    // Index covers Other.cat_id, not Document.cat_id.
    create_vertex_property_index(
        &env,
        "other_cat_id_idx",
        "Other",
        "cat_id",
        "create_other_cat_id_idx",
    );

    let v = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        1,
    );
    seed_embedding(&env, vector, env.graph_source, v.local_vertex_id, 1.0);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 1 \
           LIMIT 10 \
         ) DISTANCE AS distance \
         RETURN ELEMENT_ID(d), distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(0.0)),
    )])
    .expect("encode params");

    let err = gql_query_with_params_as_admin_result(&env, &query, params)
        .expect_err("wrong-label index coverage must fail");
    assert!(
        err.to_string().contains("vertex equality index"),
        "wrong-label index coverage must fail with a coverage error, got {err}"
    );
}

#[test]
fn search_where_equality_label_filter_wins_over_closer_other_vertex() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document");
    let other_label_id = admin_intern_vertex_label(&env, "Other");
    let cat_id = admin_intern_property(&env, "cat_id");

    // Exact index covers Document.cat_id.
    create_vertex_property_index(
        &env,
        "document_cat_id_idx",
        "Document",
        "cat_id",
        "create_document_cat_id_idx",
    );

    // Document with cat_id=1 and a far vector.
    let v_doc = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        1,
    );
    seed_embedding(&env, vector, env.graph_source, v_doc.local_vertex_id, 10.0);

    // Other vertex with cat_id=1 and a much closer vector. The property index for Document
    // must not match this vertex, so LIMIT 1 still returns the Document.
    let v_other = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        other_label_id.raw(),
        cat_id.raw(),
        1,
    );
    seed_embedding(&env, vector, env.graph_source, v_other.local_vertex_id, 0.0);

    let query = format!(
        "MATCH (d:Document)          SEARCH d IN (            VECTOR INDEX {EMBEDDING_NAME} FOR $query            WHERE d.cat_id = 1            LIMIT 1          ) DISTANCE AS distance          RETURN ELEMENT_ID(d), distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(0.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        result.row_count, 1,
        "label-scoped filter must ignore closer vertex from another label"
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("distance"),
        Some(&gleaph_gql_ic::IcWireValue::Float64(
            10.0_f64.powi(2) * 16.0_f64
        )),
        "the Document vector at 10.0 must win, not the Other vertex at 0.0"
    );
}
