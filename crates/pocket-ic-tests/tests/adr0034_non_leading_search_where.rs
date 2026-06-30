//! PocketIC coverage for ADR 0034 Slice 7: non-leading `SEARCH ... WHERE` equality filter.
//!
//! Semantics under test:
//!   C = property_index_equal(Document, category, value)
//!   H = vector_top_k(document_embedding, subjects = C, limit)
//!   output = prefix_rows INNER JOIN H ON prefix_rows[d] = H.subject
//!
//! - The filtered relation H is one global top-k computed before the prefix join.
//! - A wrong-category vertex cannot consume a qualifying top-k position.
//! - An unlinked qualifying vertex may consume a top-k slot, proving the search is not
//!   prefix-restricted.
//! - Prefix-row multiplicity is preserved.
//! - Empty candidates preserve the global aggregate contract.
//! - Missing exact index coverage or missing static label proof fail closed.

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
    gql_query_with_params_as_admin, install_federation, install_vector_canister,
};
use gleaph_router::types::{AdminAttachVectorIndexShardArgs, RegisterVectorIndexArgs};
use std::collections::BTreeMap;

const EMBEDDING_NAME: &str = "adr0034_doc_vec_nl_where";
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

fn extract_rows(result: GqlQueryResult) -> Vec<BTreeMap<String, gleaph_gql_ic::IcWireValue>> {
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

fn setup_non_leading_search_where_env(env: &FederationEnv, vector: Principal) {
    register_vector_index(env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(env, vector);

    let _author_label_id = admin_intern_vertex_label(env, "Author");
    let _doc_label_id = admin_intern_vertex_label(env, "Document");
    let _wrote_label_id = admin_intern_edge_label(env, "WROTE");
    let _cat_id = admin_intern_property(env, "category");

    create_vertex_property_index(
        env,
        "document_category_idx",
        "Document",
        "category",
        "create_document_category_idx",
    );

    // Wait for property index posting to be flushed.
    gleaph_pocket_ic_tests::drain_maintenance_via_timer(env, env.graph_source);
}
#[test]
fn non_leading_search_where_excludes_globally_nearer_wrong_category_vertex() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_non_leading_search_where_env(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let cat_id = admin_intern_property(&env, "category").raw();

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d_qual =
        e2e_insert_vertex_with_label_and_property(&env, env.graph_source, doc_label_id, cat_id, 1);
    let d_wrong =
        e2e_insert_vertex_with_label_and_property(&env, env.graph_source, doc_label_id, cat_id, 2);

    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d_qual.local_vertex_id,
        wrote_label_id,
    );

    // d_wrong is globally nearer to the query (value 0.0 vs 5.0), but its category is 2.
    seed_embedding(&env, vector, env.graph_source, d_qual.local_vertex_id, 5.0);
    seed_embedding(&env, vector, env.graph_source, d_wrong.local_vertex_id, 0.0);

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.category = 1 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN d, distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(0.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "LIMIT 1 filtered top-k must return exactly one qualifying row"
    );
    assert!(
        rows[0].contains_key("d"),
        "document binding must be present"
    );
}

/// ADR 0034 Slice 7: the vector top-k is computed globally, not restricted to vertices present in
/// the prefix rows. An unlinked qualifying vertex consumes a top-k slot, so a LIMIT 1 query whose
/// global top-1 has no matching prefix row returns no rows.
#[test]
fn non_leading_search_where_global_top_k_consumes_unlinked_qualifying_vertex() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_non_leading_search_where_env(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let cat_id = admin_intern_property(&env, "category").raw();

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d_linked =
        e2e_insert_vertex_with_label_and_property(&env, env.graph_source, doc_label_id, cat_id, 1);
    let d_unlinked =
        e2e_insert_vertex_with_label_and_property(&env, env.graph_source, doc_label_id, cat_id, 1);

    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d_linked.local_vertex_id,
        wrote_label_id,
    );

    // d_unlinked is globally nearer and consumes the single filtered top-k slot.
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_linked.local_vertex_id,
        5.0,
    );
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_unlinked.local_vertex_id,
        0.0,
    );

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.category = 1 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN a, d, distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(0.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        result.row_count, 0,
        "global top-1 has no matching prefix row, so the inner join is empty"
    );
}

/// ADR 0034 Slice 7: one hit vertex joined to multiple prefix rows produces the corresponding
/// number of output rows.
#[test]
fn non_leading_search_where_preserves_prefix_row_multiplicity() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_non_leading_search_where_env(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let cat_id = admin_intern_property(&env, "category").raw();

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let a2 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d =
        e2e_insert_vertex_with_label_and_property(&env, env.graph_source, doc_label_id, cat_id, 1);

    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d.local_vertex_id,
        wrote_label_id,
    );
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a2.local_vertex_id,
        d.local_vertex_id,
        wrote_label_id,
    );

    seed_embedding(&env, vector, env.graph_source, d.local_vertex_id, 5.0);

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.category = 1 \
           LIMIT 10 \
         ) DISTANCE AS distance \
         RETURN a, d, distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        result.row_count, 2,
        "one hit joined to two prefix rows must produce two output rows"
    );
}

/// ADR 0034 Slice 7: when the equality filter yields no candidates the vector canister is not
/// called, and a global aggregate over the empty resolved relation still returns one zero row.
#[test]
fn non_leading_search_where_empty_candidates_aggregate_returns_one_zero_row() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_non_leading_search_where_env(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let cat_id = admin_intern_property(&env, "category").raw();

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d =
        e2e_insert_vertex_with_label_and_property(&env, env.graph_source, doc_label_id, cat_id, 1);

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
           WHERE d.category = 99 \
           LIMIT 10 \
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
        "empty relation aggregate must return one row"
    );

    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    match rows[0].get("n").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(n) => assert_eq!(*n, 0, "count over empty relation is 0"),
        other => panic!("count must be Int64, got {other:?}"),
    }
}

/// ADR 0034 Slice 7: non-leading filtered search requires an active vertex property index for the
/// exact (label, property) tuple.
#[test]
fn non_leading_search_where_rejects_missing_exact_index() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let _author_label_id = admin_intern_vertex_label(&env, "Author");
    let doc_label_id = admin_intern_vertex_label(&env, "Document");
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE");
    let cat_id = admin_intern_property(&env, "category");

    // Intentionally do NOT create the exact vertex index.
    let a1 = e2e_insert_vertex_with_label(
        &env,
        env.graph_source,
        admin_intern_vertex_label(&env, "Author").raw(),
    );
    let d = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        1,
    );
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d.local_vertex_id,
        wrote_label_id.raw(),
    );
    seed_embedding(&env, vector, env.graph_source, d.local_vertex_id, 5.0);

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.category = 1 \
           LIMIT 10 \
         ) DISTANCE AS distance \
         RETURN d, distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let err = gql_query_with_params_as_admin_result(&env, &query, params)
        .expect_err("missing exact index must fail");
    assert!(
        err.to_string().contains("active vertex property index"),
        "missing exact index must fail with a coverage error, got {err}"
    );
}

/// ADR 0034 Slice 7: non-leading filtered search requires an active vertex property index for the
/// exact (label, property) tuple.
#[test]
fn non_leading_search_where_rejects_unlabeled_searched_binding() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document");
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE");
    let cat_id = admin_intern_property(&env, "category");

    create_vertex_property_index(
        &env,
        "document_category_idx",
        "Document",
        "category",
        "create_document_category_idx",
    );

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d = e2e_insert_vertex_with_label_and_property(
        &env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        1,
    );
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d.local_vertex_id,
        wrote_label_id.raw(),
    );
    seed_embedding(&env, vector, env.graph_source, d.local_vertex_id, 5.0);

    // d has no static label proof in the pattern, so Router cannot scope the property lookup.
    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.category = 1 \
           LIMIT 10 \
         ) DISTANCE AS distance \
         RETURN d, distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let err = gql_query_with_params_as_admin_result(&env, &query, params)
        .expect_err("unlabeled searched binding must fail");
    assert!(
        err.to_string().contains("statically proved label"),
        "unlabeled searched binding must fail with a label-proof error, got {err}"
    );
}
