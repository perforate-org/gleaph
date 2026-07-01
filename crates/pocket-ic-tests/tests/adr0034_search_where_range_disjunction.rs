//! PocketIC coverage for ADR 0034 Slice 17: `SEARCH ... WHERE` bounded same-property numeric range disjunction.
//!
//! Semantics under test:
//!   R_i = property_index_numeric_range(Document, price, OP_i, value_i)
//!   R   = UNION_i R_i  merged into disjoint encoded intervals
//!   C   = label_filter(R)
//!   result = vector_top_k(document_embedding, subjects = C, limit)
//!
//! - Candidate membership is the label-scoped union of postings from each range arm before vector ranking.
//! - A vertex matching any arm is included once, even if overlapping intervals would cover it twice.
//! - The Router enforces the two-to-eight arm bound and merges overlapping/touching encoded intervals.
//! - An arm whose resolved interval is empty or contradictory is silently dropped from the union.

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
    e2e_set_vertex_property, gql_query_with_params_as_admin, install_federation,
    install_vector_canister,
};
use gleaph_router::types::{AdminAttachVectorIndexShardArgs, RegisterVectorIndexArgs};

const EMBEDDING_NAME: &str = "adr0034_doc_vec_range_disjunction";
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

fn setup_search_where_range_disjunction_env(env: &FederationEnv, vector: Principal) {
    register_vector_index(env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(env, vector);

    let doc_label_id = admin_intern_vertex_label(env, "Document");
    let price_id = admin_intern_property(env, "price");
    create_vertex_property_index(
        env,
        "document_price_idx_range_disjunction",
        "Document",
        "price",
        "create_document_price_idx_range_disjunction",
    );

    // Four documents with deterministic prices and vectors so ranking is predictable.
    // price=5 -> vector 0.5, price=15 -> vector 1.5, price=25 -> vector 2.5, price=35 -> vector 3.5.
    for (price, vec_value) in [(5, 0.5_f32), (15, 1.5_f32), (25, 2.5_f32), (35, 3.5_f32)] {
        let doc = e2e_insert_vertex_with_label(env, env.graph_source, doc_label_id.raw());
        e2e_set_vertex_property(
            env,
            env.graph_source,
            doc.local_vertex_id,
            price_id.raw(),
            price,
        );
        seed_embedding(
            env,
            vector,
            env.graph_source,
            doc.local_vertex_id,
            vec_value,
        );
    }
}

fn setup_search_where_eight_way_range_disjunction_env(env: &FederationEnv, vector: Principal) {
    register_vector_index(env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(env, vector);

    let doc_label_id = admin_intern_vertex_label(env, "Document");
    let price_id = admin_intern_property(env, "price");
    create_vertex_property_index(
        env,
        "document_price_idx_range_eight_way",
        "Document",
        "price",
        "create_document_price_idx_range_eight_way",
    );

    // Create eight documents with price values that are covered by disjoint single-value ranges.
    // price=i*10, vector=i+1 so ordering is deterministic and unique per document.
    for i in 0..8 {
        let doc = e2e_insert_vertex_with_label(env, env.graph_source, doc_label_id.raw());
        e2e_set_vertex_property(
            env,
            env.graph_source,
            doc.local_vertex_id,
            price_id.raw(),
            i * 10,
        );
        seed_embedding(
            env,
            vector,
            env.graph_source,
            doc.local_vertex_id,
            i as f32 + 1.0,
        );
    }
}

fn distance_for_vec_value(value: f32) -> f64 {
    value.powi(2) as f64 * DIMS as f64
}

#[test]
fn search_where_range_disjunction_unions_two_intervals() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_range_disjunction_env(&env, vector);

    // price < 10 OR price >= 30 selects price=5 (distance from query 0.0 is 0.5) and price=35 (3.5).
    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price < 10 OR d.price >= 30 \
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
    assert_eq!(
        result.row_count, 2,
        "union of two range arms must return two rows"
    );

    let rows = extract_rows(result);
    assert_eq!(rows.len(), 2);
    let distances: Vec<f64> = rows
        .iter()
        .map(|r| match r.get("distance").expect("distance column") {
            gleaph_gql_ic::IcWireValue::Float64(d) => *d,
            other => panic!("distance must be Float64, got {other:?}"),
        })
        .collect();
    assert!(
        (distances[0] - distance_for_vec_value(0.5_f32)).abs() < 1e-6,
        "first row must be price=5"
    );
    assert!(
        (distances[1] - distance_for_vec_value(3.5_f32)).abs() < 1e-6,
        "second row must be price=35"
    );
}

#[test]
fn search_where_range_disjunction_dedupes_overlapping_intervals() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_range_disjunction_env(&env, vector);

    // Overlapping arms: price >= 10 OR price >= 20 both select price=15, 25, 35.
    // The union must return each document once.
    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price >= 10 OR d.price >= 20 \
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
    assert_eq!(
        result.row_count, 3,
        "overlapping range arms must not produce duplicate rows"
    );
}

#[test]
fn search_where_range_disjunction_merges_touching_intervals() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_range_disjunction_env(&env, vector);

    // Two touching one-sided range arms: price < 10 selects price=5; price >= 10 selects price=15.
    // The merge should produce a single encoded interval covering [min_numeric, +∞) and therefore
    // include the out-of-range price=25 and price=35 as well. Use a count query to avoid ordering
    // assumptions after merge.
    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price < 10 OR d.price >= 10 \
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
    assert_eq!(result.row_count, 1, "aggregate must return one row");
    let rows = extract_rows(result);
    match rows[0].get("n").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(v) => {
            assert_eq!(*v, 4, "touching intervals must merge to all rows")
        }
        other => panic!("count must be Int64 4, got {other:?}"),
    }
}

#[test]
fn search_where_range_disjunction_excludes_gap() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_range_disjunction_env(&env, vector);

    // price < 10 OR price >= 30 excludes price=15 and price=25.
    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price < 10 OR d.price >= 30 \
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
    assert_eq!(result.row_count, 1, "aggregate must return one row");
    let rows = extract_rows(result);
    match rows[0].get("n").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(v) => assert_eq!(*v, 2, "count must be two"),
        other => panic!("count must be Int64 2, got {other:?}"),
    }
}

#[test]
fn search_where_range_disjunction_with_unmatched_arm() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_range_disjunction_env(&env, vector);

    // One arm matches nothing, the other matches price=5.
    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price < 0 OR d.price < 10 \
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
    assert_eq!(result.row_count, 1, "aggregate must return one row");
    let rows = extract_rows(result);
    match rows[0].get("n").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(v) => assert_eq!(*v, 1, "count must be one"),
        other => panic!("count must be Int64 1, got {other:?}"),
    }
}

#[test]
fn search_where_eight_way_range_disjunction_unions_all_values() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_eight_way_range_disjunction_env(&env, vector);

    // Each arm is a single one-sided range comparison. Documents are seeded with prices 0,10,20,...,70.
    let arms: Vec<String> = (0..8)
        .map(|i| {
            let low = i * 10;
            format!("d.price >= {low}")
        })
        .collect();
    let where_clause = arms.join(" OR ");
    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE {where_clause} \
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
    assert_eq!(
        result.row_count, 8,
        "eight-way range disjunction must return all eight documents"
    );
}

#[test]
fn search_where_nine_arm_range_disjunction_is_rejected() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_eight_way_range_disjunction_env(&env, vector);

    // Each arm is a single one-sided range comparison so the disjunction shape is accepted.
    let arms: Vec<String> = (0..9)
        .map(|i| {
            let low = i * 10;
            format!("d.price >= {low}")
        })
        .collect();
    let where_clause = arms.join(" OR ");
    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE {where_clause} \
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
        .expect_err("nine range disjunction arms must be rejected");
    assert!(
        err.to_string().contains("at most 8")
            || err
                .to_string()
                .contains("range disjunction supports at most"),
        "nine arms must fail with an explicit arm-count error, got {err}"
    );
}

#[test]
fn search_where_range_disjunction_rejects_missing_index() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document");
    let price_id = admin_intern_property(&env, "price");
    let doc = e2e_insert_vertex_with_label(&env, env.graph_source, doc_label_id.raw());
    e2e_set_vertex_property(
        &env,
        env.graph_source,
        doc.local_vertex_id,
        price_id.raw(),
        5,
    );
    seed_embedding(&env, vector, env.graph_source, doc.local_vertex_id, 1.0);

    // No property index for price on Document.
    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price < 10 OR d.price >= 20 \
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
        .expect_err("range disjunction without active index must be rejected");
    assert!(
        err.to_string().contains("active vertex property index")
            || err.to_string().contains("Unsupported"),
        "missing index must fail with a coverage error, got {err}"
    );
}

#[test]
fn search_where_range_disjunction_with_parameterized_predicates() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_range_disjunction_env(&env, vector);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price < $low OR d.price >= $high \
           LIMIT 10 \
         ) DISTANCE AS distance \
         RETURN count(*) AS n"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![
        ("query".to_string(), Value::Bytes(vec_bytes(0.0))),
        ("low".to_string(), Value::Int64(10)),
        ("high".to_string(), Value::Int64(30)),
    ])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(result.row_count, 1, "aggregate must return one row");
    let rows = extract_rows(result);
    match rows[0].get("n").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(v) => assert_eq!(*v, 2, "count must be two"),
        other => panic!("count must be Int64 2, got {other:?}"),
    }
}

#[test]
fn search_where_range_disjunction_empty_candidate_set_aggregate() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_range_disjunction_env(&env, vector);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price < 0 OR d.price >= 100 \
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
        "empty disjunction candidate set must still produce one aggregate row"
    );
    let rows = extract_rows(result);
    match rows[0].get("n").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(v) => assert_eq!(*v, 0, "count must be zero"),
        other => panic!("count must be Int64 0, got {other:?}"),
    }
}

#[test]
fn non_leading_search_where_range_disjunction_unions_values() {
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
        "document_price_idx_non_leading",
        "Document",
        "price",
        "create_document_price_idx_non_leading",
    );

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let a2 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);

    // d_low matches only the lower arm (price < 10), d_high only the upper arm (price >= 20),
    // and d_gap matches neither.
    let d_low = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        5,
        price_id.raw(),
        5,
    );
    let d_high = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        25,
        price_id.raw(),
        25,
    );
    let d_gap = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        price_id.raw(),
        15,
        price_id.raw(),
        15,
    );

    for a in [a1.local_vertex_id, a2.local_vertex_id] {
        e2e_insert_edge_with_label(
            &env,
            env.graph_source,
            a,
            d_low.local_vertex_id,
            wrote_label_id,
        );
        e2e_insert_edge_with_label(
            &env,
            env.graph_source,
            a,
            d_high.local_vertex_id,
            wrote_label_id,
        );
        e2e_insert_edge_with_label(
            &env,
            env.graph_source,
            a,
            d_gap.local_vertex_id,
            wrote_label_id,
        );
    }

    // Make the gap vertex globally nearest so that ignoring the filter would otherwise include it.
    seed_embedding(&env, vector, env.graph_source, d_low.local_vertex_id, 1.0);
    seed_embedding(&env, vector, env.graph_source, d_high.local_vertex_id, 2.0);
    seed_embedding(&env, vector, env.graph_source, d_gap.local_vertex_id, 0.0);

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.price < 10 OR d.price >= 20 \
           LIMIT 10 \
         ) DISTANCE AS distance \
         RETURN a, d, distance ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![(
        "query".to_string(),
        Value::Bytes(vec_bytes(0.0)),
    )])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(
        result.row_count, 4,
        "two authors joined to two surviving documents produce four rows"
    );

    let rows = extract_rows(result);
    assert_eq!(rows.len(), 4);
    for row in &rows {
        let distance = match row.get("distance").expect("distance column") {
            gleaph_gql_ic::IcWireValue::Float64(d) => *d,
            other => panic!("distance must be Float64, got {other:?}"),
        };
        assert!(
            (distance - 1.0_f64.powi(2) * 16.0_f64).abs() < 1e-6
                || (distance - 2.0_f64.powi(2) * 16.0_f64).abs() < 1e-6,
            "only documents with price < 10 or price >= 20 may appear, got distance {distance}"
        );
    }
}
