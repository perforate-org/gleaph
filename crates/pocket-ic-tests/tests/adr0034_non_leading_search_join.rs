//! PocketIC coverage for ADR 0034 Slice 5: one top-level non-leading `SEARCH` joined against
//! already-bound graph vertex rows.
//!
//! Semantics under test:
//!   H = vector_search(index, query, limit)
//!   output = input_rows INNER JOIN H ON input_rows[d] = H.subject
//!
//! - Vector search runs exactly once per GQL execution.
//! - Global top-k is computed before the join, so the final result may contain fewer than k
//!   surviving subjects and may contain more rows than k when one hit joins to multiple graph rows.
//! - `L2Squared` emits `DISTANCE AS`; exact-scan `Cosine` emits `SCORE AS`.
//! - Row multiplicity is preserved: a hit vertex with N incoming graph rows produces N output rows.

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
    FederationEnv, GRAPH_NAME, admin_intern_edge_label, admin_intern_vertex_label,
    e2e_insert_edge_with_label, e2e_insert_vertex_with_label, gql_query_with_params_as_admin,
    install_federation, install_vector_canister,
};
use gleaph_router::types::{AdminAttachVectorIndexShardArgs, RegisterVectorIndexArgs};
use std::collections::BTreeMap;

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
    let _: bool = Decode!(&bytes,
    Result<bool, RouterError>)
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

fn seed_embedding(
    env: &FederationEnv,
    vector: Principal,
    shard_canister: Principal,
    vertex_id: u32,
    value: f32,
    metric: VectorMetric,
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
        metric,
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
    let _: () = Decode!(&bytes, Result<(), gleaph_graph_kernel::vector_index::VectorIndexError>)
        .expect("decode upsert result")
        .expect("seed embedding");
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

fn extract_rows(result: GqlQueryResult) -> Vec<BTreeMap<String, gleaph_gql_ic::IcWireValue>> {
    let rows_blob = result.rows_blob.expect("rows blob");
    let wire = IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode rows");
    wire.rows
        .into_iter()
        .map(|row| row.columns.into_iter().collect())
        .collect()
}

/// ADR 0034 Slice 5: a non-leading `SEARCH` after a graph prefix returns only rows whose bound
/// vertex is in the global vector top-k, preserves multiplicity, and emits `DISTANCE AS` for an
/// `L2Squared` index.
#[test]
fn non_leading_search_join_preserves_multiplicity_l2_distance() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    // Intern labels so the GQL planner can resolve them.
    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Doc").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();

    // Insert two authors and one document on shard 0; connect both authors to the same doc.
    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let a2 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d = e2e_insert_vertex_with_label(&env, env.graph_source, doc_label_id);
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

    // Seed the document embedding so it is the sole vector hit.
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d.local_vertex_id,
        5.0,
        VectorMetric::L2Squared,
    );

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Doc) \
         SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) DISTANCE AS distance \
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
        "one hit vertex joined to two graph rows must produce two output rows"
    );

    let rows = extract_rows(result);
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert!(row.contains_key("a"), "author binding must be present");
        assert!(row.contains_key("d"), "document binding must be present");
        match row.get("distance").expect("distance column") {
            gleaph_gql_ic::IcWireValue::Float64(d) => {
                assert!(
                    (d - 0.0f64).abs() < 1e-6,
                    "exact match distance must be 0.0"
                );
            }
            other => panic!("distance must be Float64, got {other:?}"),
        }
    }
}

/// ADR 0034 Slice 5: `SCORE AS` on an exact-scan `Cosine` index executes end-to-end for a
/// non-leading search and returns a score alias.
#[test]
fn non_leading_search_join_score_as_cosine() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::Cosine, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Doc").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let d = e2e_insert_vertex_with_label(&env, env.graph_source, doc_label_id);
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a1.local_vertex_id,
        d.local_vertex_id,
        wrote_label_id,
    );

    // Constant vectors are identical direction; score should be 1.0.
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d.local_vertex_id,
        5.0,
        VectorMetric::Cosine,
    );

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Doc) \
         SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) SCORE AS score \
         RETURN a, d, score ORDER BY score DESC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(5.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(result.row_count, 1);

    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    match rows[0].get("score").expect("score column") {
        gleaph_gql_ic::IcWireValue::Float64(score) => {
            assert!(
                (score - 1.0f64).abs() < 1e-6,
                "identical directions score ~1.0"
            );
        }
        other => panic!("score must be Float64, got {other:?}"),
    }
}

/// ADR 0034 Slice 5: `LIMIT` applies to the global vector top-k before the join, so a query
/// can return fewer than k surviving subjects and the surviving subject can still multiply
/// across graph rows.
#[test]
fn non_leading_search_global_top_k_computed_before_join() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Doc").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();

    // Two documents: d_near is the exact match, d_far is farther away.
    let d_near = e2e_insert_vertex_with_label(&env, env.graph_source, doc_label_id);
    let d_far = e2e_insert_vertex_with_label(&env, env.graph_source, doc_label_id);

    // Two authors point to d_near; one author points to d_far.
    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let a2 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let a3 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
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
        a2.local_vertex_id,
        d_near.local_vertex_id,
        wrote_label_id,
    );
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        a3.local_vertex_id,
        d_far.local_vertex_id,
        wrote_label_id,
    );

    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_near.local_vertex_id,
        5.0,
        VectorMetric::L2Squared,
    );
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_far.local_vertex_id,
        100.0,
        VectorMetric::L2Squared,
    );

    // LIMIT 1 means global top-k contains only d_near. The join must still produce two rows
    // (one per author pointing to d_near), proving top-k is global and multiplicity is preserved.
    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Doc) \
         SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 1) DISTANCE AS distance \
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
                    "only the exact-match document survives global top-k"
                );
            }
            other => panic!("distance must be Float64, got {other:?}"),
        }
    }
}

/// ADR 0034 Slice 5: `DISTANCE AS` on a cosine index is rejected for a non-leading search,
/// matching the leading-search metric/output-shape contract.
#[test]
fn non_leading_search_distance_as_rejected_for_cosine() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::Cosine, vector);
    enable_vector_dispatch(&env, vector);

    admin_intern_vertex_label(&env, "Author");
    admin_intern_vertex_label(&env, "Doc");
    let _wrote_label_id = admin_intern_edge_label(&env, "WROTE");

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Doc) \
         SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) DISTANCE AS distance \
         RETURN a, d, distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(1.0)),
    )])
    .expect("encode params");

    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "gql_query",
            Encode!(&query.to_string(), &params).expect("encode gql_query"),
        )
        .expect("gql_query call");
    let result: Result<GqlQueryResult, RouterError> =
        Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_query result");
    let err = result.expect_err("DISTANCE AS on cosine must fail");
    assert!(
        err.to_string().contains("not supported for metric"),
        "unexpected error: {err}"
    );
}

/// ADR 0034 Slice 5: a `SEARCH` whose subject is not bound by the preceding graph prefix is
/// rejected (it would require a different execution model).
#[test]
fn non_leading_search_rejects_unbound_subject() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    admin_intern_vertex_label(&env, "Author");

    // The prefix binds `a`, but `SEARCH d` references an unbound variable `d`.
    let query = format!(
        "MATCH (a:Author) \
         SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR $query LIMIT 10) DISTANCE AS distance \
         RETURN a, distance"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(1.0)),
    )])
    .expect("encode params");

    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "gql_query",
            Encode!(&query.to_string(), &params).expect("encode gql_query"),
        )
        .expect("gql_query call");
    let result: Result<GqlQueryResult, RouterError> =
        Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_query result");
    assert!(
        result.is_err(),
        "non-leading SEARCH on an unbound variable must fail: {result:?}"
    );
}
