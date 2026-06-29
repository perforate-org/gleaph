//! PocketIC coverage for ADR 0034 Slice 8: `SEARCH ... WHERE` equality conjunction.
//!
//! Semantics under test:
//!   C1 = property_index_equal(Document, cat_id, value)
//!   C2 = property_index_equal(Document, tenant_id, value)
//!   C  = C1 INTERSECT C2
//!   result = vector_top_k(document_embedding, subjects = C, limit)
//!
//! - Candidate membership is the exact label-scoped Property Index intersection before vector ranking.
//! - A vertex matching only one arm cannot consume a top-k position, even if it is globally nearer.

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
    e2e_insert_vertex_with_label, e2e_insert_vertex_with_label_and_two_properties,
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
    Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_query result")
}

fn setup_search_where_conjunction_env(env: &FederationEnv, vector: Principal) {
    register_vector_index(env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(env, vector);

    let doc_label_id = admin_intern_vertex_label(env, "Document");
    let cat_id = admin_intern_property(env, "cat_id");
    let tenant_id = admin_intern_property(env, "tenant_id");
    create_vertex_property_index(
        env,
        "document_cat_id_idx",
        "Document",
        "cat_id",
        "create_document_cat_id_idx",
    );
    create_vertex_property_index(
        env,
        "document_tenant_id_idx",
        "Document",
        "tenant_id",
        "create_document_tenant_id_idx",
    );

    // Insert three documents:
    // - doc_match:  cat=1, tenant=1, vector 2.0 (distance 4 from query 0.0)
    // - doc_one:    cat=1, tenant=2, vector 0.0 (distance 0) — matches only one arm
    // - doc_other:  cat=2, tenant=1, vector 0.0 (distance 0) — matches only one arm
    let doc_match = e2e_insert_vertex_with_label_and_two_properties(
        env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        1,
        tenant_id.raw(),
        1,
    );
    let doc_one = e2e_insert_vertex_with_label_and_two_properties(
        env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        1,
        tenant_id.raw(),
        2,
    );
    let doc_other = e2e_insert_vertex_with_label_and_two_properties(
        env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        2,
        tenant_id.raw(),
        1,
    );

    seed_embedding(
        env,
        vector,
        env.graph_source,
        doc_match.local_vertex_id,
        2.0,
    );
    seed_embedding(env, vector, env.graph_source, doc_one.local_vertex_id, 0.0);
    seed_embedding(
        env,
        vector,
        env.graph_source,
        doc_other.local_vertex_id,
        0.0,
    );
}

#[test]
fn search_where_conjunction_excludes_globally_nearer_one_arm_vertices() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_conjunction_env(&env, vector);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 1 AND d.tenant_id = 1 \
           LIMIT 1 \
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
    assert_eq!(
        result.row_count, 1,
        "only the document matching both arms should survive intersection"
    );

    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("distance"),
        Some(&gleaph_gql_ic::IcWireValue::Float64(
            2.0_f64.powi(2) * 16.0_f64
        )),
        "doc_match at 2.0 must win, not the nearer one-arm vertices at 0.0"
    );
}

#[test]
fn search_where_conjunction_parameter_predicates() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_conjunction_env(&env, vector);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = $cat AND d.tenant_id = $tenant \
           LIMIT 10 \
         ) DISTANCE AS distance \
         RETURN ELEMENT_ID(d), distance \
         ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![
        ("query".to_string(), Value::Bytes(vec_bytes(0.0))),
        ("cat".to_string(), Value::Int64(1)),
        ("tenant".to_string(), Value::Int64(1)),
    ])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(result.row_count, 1, "expected one intersection match");

    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
}

#[test]
fn search_where_conjunction_empty_result_aggregate() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_conjunction_env(&env, vector);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 999 AND d.tenant_id = 1 \
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
fn search_where_conjunction_rejects_missing_second_exact_index() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document");
    let cat_id = admin_intern_property(&env, "cat_id");
    let tenant_id = admin_intern_property(&env, "tenant_id");
    // Only cat_id has an active index for Document; tenant_id does not.
    create_vertex_property_index(
        &env,
        "document_cat_id_idx",
        "Document",
        "cat_id",
        "create_document_cat_id_idx",
    );

    let v = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        1,
        tenant_id.raw(),
        1,
    );
    seed_embedding(&env, vector, env.graph_source, v.local_vertex_id, 1.0);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 1 AND d.tenant_id = 1 \
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
        .expect_err("missing second exact index must fail");
    assert!(
        err.to_string().contains("vertex equality index"),
        "missing second exact index must fail with a coverage error, got {err}"
    );
}

#[test]
fn non_leading_search_where_conjunction_excludes_globally_nearer_one_arm_vertices() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let cat_id = admin_intern_property(&env, "cat_id");
    let tenant_id = admin_intern_property(&env, "tenant_id");

    create_vertex_property_index(
        &env,
        "document_cat_id_idx",
        "Document",
        "cat_id",
        "create_document_cat_id_idx",
    );
    create_vertex_property_index(
        &env,
        "document_tenant_id_idx",
        "Document",
        "tenant_id",
        "create_document_tenant_id_idx",
    );

    // Two authors, both connected to the same two documents.
    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let a2 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);

    // d_match matches both filter arms and has the same vector as the query.
    let d_match = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        cat_id.raw(),
        1,
        tenant_id.raw(),
        1,
    );
    // d_one matches only the cat_id arm and is globally nearer (vector 0 vs query 5).
    let d_one = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        cat_id.raw(),
        1,
        tenant_id.raw(),
        2,
    );

    for a in [a1.local_vertex_id, a2.local_vertex_id] {
        e2e_insert_edge_with_label(
            &env,
            env.graph_source,
            a,
            d_match.local_vertex_id,
            wrote_label_id,
        );
        e2e_insert_edge_with_label(
            &env,
            env.graph_source,
            a,
            d_one.local_vertex_id,
            wrote_label_id,
        );
    }

    seed_embedding(&env, vector, env.graph_source, d_match.local_vertex_id, 5.0);
    seed_embedding(&env, vector, env.graph_source, d_one.local_vertex_id, 0.0);

    // LIMIT 1 global top-k, restricted to the intersection, must contain only d_match.
    // Both author rows pointing to d_match survive the inner join.
    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 1 AND d.tenant_id = 1 \
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
    assert_eq!(
        result.row_count, 2,
        "LIMIT 1 global top-k joins to the surviving document's two author rows"
    );

    let rows = extract_rows(result);
    assert_eq!(rows.len(), 2);
    for row in &rows {
        match row.get("distance").expect("distance column") {
            gleaph_gql_ic::IcWireValue::Float64(d) => {
                assert!(
                    (d - 0.0f64).abs() < 1e-6,
                    "only the exact-match intersection document survives global top-k"
                );
            }
            other => panic!("distance must be Float64, got {other:?}"),
        }
    }
}

#[test]
fn non_leading_search_where_conjunction_empty_result_aggregate() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let cat_id = admin_intern_property(&env, "cat_id");
    let tenant_id = admin_intern_property(&env, "tenant_id");

    create_vertex_property_index(
        &env,
        "document_cat_id_idx",
        "Document",
        "cat_id",
        "create_document_cat_id_idx",
    );
    create_vertex_property_index(
        &env,
        "document_tenant_id_idx",
        "Document",
        "tenant_id",
        "create_document_tenant_id_idx",
    );

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        cat_id.raw(),
        1,
        tenant_id.raw(),
        1,
    );
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d.local_vertex_id,
        wrote_label_id,
    );
    seed_embedding(&env, vector, env.graph_source, d.local_vertex_id, 1.0);

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 999 AND d.tenant_id = 1 \
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
        "empty filtered non-leading search must still produce one aggregate row"
    );

    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    match rows[0].get("n").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(v) => assert_eq!(*v, 0, "count must be zero"),
        other => panic!("count must be Int64 0, got {other:?}"),
    }
}
