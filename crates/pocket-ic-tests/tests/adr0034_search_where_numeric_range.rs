//! PocketIC coverage for ADR 0034 Slices 9, 10 and 11: `SEARCH ... WHERE` numeric range, two-sided numeric range, and mixed equality-plus-range.
//!
//! Semantics under test:
//!   R = property_index_numeric_range(Document, price, OP, value)
//!   result = vector_top_k(document_embedding, subjects = R, limit)
//!
//! - Candidate membership is the exact label-scoped numeric range before vector ranking.
//! - A globally nearer vertex with an out-of-range or non-numeric value for the same property, or with a value outside a two-sided interval,
//!   must not consume a top-k position.

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
    FederationEnv, GRAPH_NAME, admin_intern_edge_label, admin_intern_property,
    admin_intern_vertex_label, create_vertex_property_index, e2e_insert_edge_with_label,
    e2e_insert_vertex_with_label, e2e_insert_vertex_with_label_and_property,
    e2e_insert_vertex_with_label_and_two_properties, gql_query_with_params_as_admin,
    install_federation, install_vector_canister,
};
use gleaph_router::types::{AdminAttachVectorIndexShardArgs, RegisterVectorIndexArgs};

const EMBEDDING_NAME: &str = "adr0034_doc_vec_range";
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
    let _: () = Decode!(&bytes, Result<(), RouterError>)
        .expect("decode upsert result")
        .expect("upsert embedding");
}

fn assert_row_count_and_distance(
    result: GqlQueryResult,
    expected_count: usize,
    expected_distance: f64,
) {
    assert_eq!(
        result.row_count, expected_count as u64,
        "row count mismatch"
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), expected_count);
    for row in &rows {
        match row.get("distance").expect("distance column") {
            gleaph_gql_ic::IcWireValue::Float64(d) => {
                assert!(
                    (d - expected_distance).abs() < 1e-6,
                    "distance mismatch: {d}"
                );
            }
            other => panic!("distance must be Float64, got {other:?}"),
        }
    }
}

fn assert_row_count(result: GqlQueryResult, expected_count: usize) {
    assert_eq!(
        result.row_count, expected_count as u64,
        "row count mismatch"
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), expected_count);
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

#[test]
fn search_where_numeric_range_excludes_out_of_range_and_missing_property() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);

    // d_match has a price in range and a vector farther than the excluded vertices.
    let d_match = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        7,
    );
    // d_near is globally nearer but price is out of range.
    let d_near = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        3,
    );
    // d_none has the same label but no price value; it must not enter the range.
    let d_none = e2e_insert_vertex_with_label(&env, env.graph_source, doc_label_id);

    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d_match.local_vertex_id,
        wrote_label_id,
    );
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d_near.local_vertex_id,
        wrote_label_id,
    );
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d_none.local_vertex_id,
        wrote_label_id,
    );

    // Embeddings: query=5.0, d_match=10.0 -> distance 400, d_near=5.0 -> distance 0.
    // If the range filter failed, d_near would win; the correct range keeps it out.
    // d_none=6.0 -> distance 16, but it has no price posting and is excluded.
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_match.local_vertex_id,
        10.0,
    );
    seed_embedding(&env, vector, env.graph_source, d_near.local_vertex_id, 5.0);
    seed_embedding(&env, vector, env.graph_source, d_none.local_vertex_id, 6.0);

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price >= 5 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN a, d, distance ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_row_count_and_distance(result, 1, 400.0);
}

#[test]
fn non_leading_search_where_numeric_range_parameter_predicate() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d_match = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        10,
    );
    let d_too_cheap = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        4,
    );

    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d_match.local_vertex_id,
        wrote_label_id,
    );
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d_too_cheap.local_vertex_id,
        wrote_label_id,
    );

    // d_too_cheap is closer (distance 25 vs 100) but below the parameterized threshold.
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_match.local_vertex_id,
        10.0,
    );
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_too_cheap.local_vertex_id,
        5.0,
    );

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price > $min_price \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN a, d, distance ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![
        ("query".to_string(), Value::Bytes(vec_bytes(5.0))),
        ("min_price".to_string(), Value::Int64(5)),
    ])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_row_count_and_distance(result, 1, 400.0);
}

#[test]
fn search_where_numeric_range_empty_aggregate_returns_zero_rows() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        2,
    );
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d.local_vertex_id,
        wrote_label_id,
    );
    seed_embedding(&env, vector, env.graph_source, d.local_vertex_id, 5.0);

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price >= 10 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN count(*) AS n"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        result.row_count, 1,
        "empty range aggregate must return exactly one aggregate row"
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    match rows[0].get("n").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(v) => assert_eq!(*v, 0, "count must be zero"),
        other => panic!("count must be Int64, got {other:?}"),
    }
}

#[test]
fn search_where_numeric_range_rejects_missing_index() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let _price_id = admin_intern_property(&env, "price");
    // No property index is created for price.

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d = e2e_insert_vertex_with_label(&env, env.graph_source, doc_label_id);
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d.local_vertex_id,
        wrote_label_id,
    );

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price >= 1 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN a, d, distance ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin_result(&env, &query, params);
    let message = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected error result"),
    };
    assert!(
        message.contains("requires an active vertex property index"),
        "unexpected error: {message}"
    );
}

#[test]
fn search_where_numeric_range_leading_excludes_out_of_range() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    let d_match = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        7,
    );
    let d_near = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        3,
    );

    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_match.local_vertex_id,
        10.0,
    );
    seed_embedding(&env, vector, env.graph_source, d_near.local_vertex_id, 5.0);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price >= 5 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN d, distance ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_row_count_and_distance(result, 1, 400.0);
}

#[test]
fn search_where_numeric_range_operators_are_exact() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    let d_at = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        5,
    );
    let d_above = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        6,
    );

    // Both use the same vector value so operator semantics, not distance, decide inclusion.
    seed_embedding(&env, vector, env.graph_source, d_at.local_vertex_id, 10.0);
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_above.local_vertex_id,
        10.0,
    );

    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let run = |op: &str, expected: usize| {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price {op} 5 \
               LIMIT 2 \
             ) DISTANCE AS distance \
             RETURN d, distance ORDER BY distance ASC"
        );
        let result = gql_query_with_params_as_admin(&env, &query, params.clone());
        assert_row_count(result, expected);
    };

    run(">=", 2); // 5 and 6
    run(">", 1); // 6 only
    run("<=", 1); // 5 only
    run("<", 0); // neither
}

#[test]
fn search_where_numeric_range_non_leading_preserves_multiplicity() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let a2 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d_match = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        7,
    );

    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d_match.local_vertex_id,
        wrote_label_id,
    );
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a2.local_vertex_id,
        d_match.local_vertex_id,
        wrote_label_id,
    );

    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_match.local_vertex_id,
        10.0,
    );

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price >= 5 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN a, d, distance ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    // One global top-k hit joined against two prefix rows produces two output rows.
    assert_row_count(result, 2);
}

#[test]
fn search_where_numeric_range_rejects_different_property_conjunction() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let _price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    let d = e2e_insert_vertex_with_label(&env, env.graph_source, doc_label_id);
    seed_embedding(&env, vector, env.graph_source, d.local_vertex_id, 5.0);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price >= 5 AND d.score < 100 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN d, distance ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin_result(&env, &query, params);
    let message = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected error result"),
    };
    assert!(
        message.contains("same property"),
        "different-property range conjunction must be rejected, got {message}"
    );
}

#[test]
fn search_where_numeric_range_two_sided_excludes_out_of_range() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    // d_match is in [5, 10) but farther from the query than d_near, which is below the lower bound.
    let d_match = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        7,
    );
    let d_near = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        3,
    );
    let d_above = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        10,
    );

    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_match.local_vertex_id,
        10.0,
    );
    seed_embedding(&env, vector, env.graph_source, d_near.local_vertex_id, 5.0);
    seed_embedding(&env, vector, env.graph_source, d_above.local_vertex_id, 5.0);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price >= 5 AND d.price < 10 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN d, distance ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_row_count_and_distance(result, 1, 400.0);
}

#[test]
fn search_where_numeric_range_two_sided_non_leading() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d_match = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        7,
    );
    let d_near = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        3,
    );

    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d_match.local_vertex_id,
        wrote_label_id,
    );
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d_near.local_vertex_id,
        wrote_label_id,
    );

    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_match.local_vertex_id,
        10.0,
    );
    seed_embedding(&env, vector, env.graph_source, d_near.local_vertex_id, 5.0);

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price >= 5 AND d.price < 10 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN a, d, distance ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_row_count_and_distance(result, 1, 400.0);
}

#[test]
fn search_where_numeric_range_two_sided_empty_intersection_returns_zero() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        7,
    );
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d.local_vertex_id,
        wrote_label_id,
    );
    seed_embedding(&env, vector, env.graph_source, d.local_vertex_id, 5.0);

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price > 10 AND d.price <= 10 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN count(*) AS n"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        result.row_count, 1,
        "empty two-sided intersection aggregate must return exactly one aggregate row"
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    match rows[0].get("n").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(v) => assert_eq!(*v, 0, "count must be zero"),
        other => panic!("count must be Int64, got {other:?}"),
    }
}

#[test]
fn search_where_numeric_range_two_sided_endpoint_strictness() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    // Prices 5, 6, 7, 10; identical vectors so only endpoint semantics affect inclusion.
    let d_5 = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        5,
    );
    let d_6 = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        6,
    );
    let d_7 = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        7,
    );
    let d_10 = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        10,
    );

    seed_embedding(&env, vector, env.graph_source, d_5.local_vertex_id, 10.0);
    seed_embedding(&env, vector, env.graph_source, d_6.local_vertex_id, 10.0);
    seed_embedding(&env, vector, env.graph_source, d_7.local_vertex_id, 10.0);
    seed_embedding(&env, vector, env.graph_source, d_10.local_vertex_id, 10.0);

    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let run = |lower_op: &str, upper_op: &str, expected: usize| {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price {lower_op} 5 AND d.price {upper_op} 10 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN d, distance ORDER BY distance ASC"
        );
        let result = gql_query_with_params_as_admin(&env, &query, params.clone());
        assert_row_count(result, expected);
    };

    // [5, 10) -> 5, 6, 7
    run(">=", "<", 3);
    // [5, 10] -> 5, 6, 7, 10
    run(">=", "<=", 4);
    // (5, 10) -> 6, 7
    run(">", "<", 2);
    // (5, 10] -> 6, 7, 10
    run(">", "<=", 3);
}

#[test]
fn search_where_numeric_range_two_sided_equal_endpoint() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    // Prices 4, 5, 6; identical vectors so only endpoint semantics affect inclusion.
    let d_4 = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        4,
    );
    let d_5 = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        5,
    );
    let d_6 = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        6,
    );

    seed_embedding(&env, vector, env.graph_source, d_4.local_vertex_id, 5.0);
    seed_embedding(&env, vector, env.graph_source, d_5.local_vertex_id, 5.0);
    seed_embedding(&env, vector, env.graph_source, d_6.local_vertex_id, 5.0);

    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price >= 5 AND d.price <= 5 \
           LIMIT 10 \
         ) DISTANCE AS distance \
         RETURN count(*) AS n"
    );

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        result.row_count, 1,
        "equal inclusive endpoint must return exactly one aggregate row"
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    match rows[0].get("n").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(v) => assert_eq!(*v, 1, "count must be exactly one"),
        other => panic!("count must be Int64, got {other:?}"),
    }
}

#[test]
fn search_where_numeric_range_two_sided_reversed_operands() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    let d_match = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        7,
    );
    let d_out = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        3,
    );

    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_match.local_vertex_id,
        10.0,
    );
    seed_embedding(&env, vector, env.graph_source, d_out.local_vertex_id, 5.0);

    // 10 > d.price >= 5 normalizes to d.price < 10 and d.price >= 5.
    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE 10 > d.price AND d.price >= 5 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN d, distance ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_row_count_and_distance(result, 1, 400.0);
}

#[test]
fn search_where_mixed_equality_and_range_excludes_single_arm_matches() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let category_id = admin_intern_property(&env, "category");
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_category_idx",
        "Document",
        "category",
        "create_document_category_idx",
    );
    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);

    // d_match: category=1 AND price=7 (>= 5) -> should win.
    let d_match = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        category_id.raw(),
        1,
        price_id.raw(),
        7,
    );

    // d_eq_only: category=1 but price=3 -> excluded by range.
    let d_eq_only = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        category_id.raw(),
        1,
        price_id.raw(),
        3,
    );

    // d_range_only: price=7 but category=2 -> excluded by equality.
    let d_range_only = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        category_id.raw(),
        2,
        price_id.raw(),
        7,
    );

    for d in [&d_match, &d_eq_only, &d_range_only] {
        e2e_insert_edge_with_label(
            &env,
            env.graph_source,
            a1.local_vertex_id,
            d.local_vertex_id,
            wrote_label_id,
        );
    }

    // d_match satisfies both arms but has the worst raw vector distance. The single-arm-only
    // vertices are strictly nearer, so if either filter arm is ignored they will win the top-1
    // distance ordering.
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_match.local_vertex_id,
        10.0,
    );
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_eq_only.local_vertex_id,
        5.0,
    );
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_range_only.local_vertex_id,
        5.0,
    );

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.category = 1 AND d.price >= 5 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN a, d, distance ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_row_count_and_distance(result, 1, 400.0);
}

#[test]
fn search_where_mixed_equality_and_range_parameter_reversed_order() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let category_id = admin_intern_property(&env, "category");
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_category_idx",
        "Document",
        "category",
        "create_document_category_idx",
    );
    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    let d_match = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        category_id.raw(),
        1,
        price_id.raw(),
        7,
    );
    let d_out = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        category_id.raw(),
        1,
        price_id.raw(),
        3,
    );

    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_match.local_vertex_id,
        10.0,
    );
    seed_embedding(&env, vector, env.graph_source, d_out.local_vertex_id, 5.0);

    // Reversed conjunct and operand order: $min_price <= d.price AND $category = d.category.
    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE $min_price <= d.price AND $category = d.category \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN d, distance ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![
        ("query".to_string(), Value::Bytes(vec_bytes(5.0))),
        ("min_price".to_string(), Value::Int64(5)),
        ("category".to_string(), Value::Int64(1)),
    ])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_row_count_and_distance(result, 1, 400.0);
}

#[test]
fn search_where_mixed_equality_and_range_empty_aggregate() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let category_id = admin_intern_property(&env, "category");
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_category_idx",
        "Document",
        "category",
        "create_document_category_idx",
    );
    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    // A Document with category=1 but price below the range: intersection is empty.
    let d = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        category_id.raw(),
        1,
        price_id.raw(),
        3,
    );
    seed_embedding(&env, vector, env.graph_source, d.local_vertex_id, 10.0);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.category = 1 AND d.price >= 5 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN count(*) AS n"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        result.row_count, 1,
        "empty intersection must produce one aggregate row"
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    match rows[0].get("n").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(v) => assert_eq!(*v, 0, "count must be zero"),
        other => panic!("count must be Int64, got {other:?}"),
    }
}

#[test]
fn search_where_mixed_equality_and_range_rejects_missing_category_index() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let _category_id = admin_intern_property(&env, "category");
    let price_id = admin_intern_property(&env, "price");

    // Only the range index exists; category is interned but not indexed.
    create_vertex_property_index(
        &env,
        "document_price_idx",
        "Document",
        "price",
        "create_document_price_idx",
    );

    e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        7,
    );

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.category = 1 AND d.price >= 5 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN d, distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin_result(&env, &query, params);
    let err = result.expect_err("missing category index must fail");
    assert!(
        err.to_string().contains("active vertex property index"),
        "unexpected error: {err}"
    );
}
