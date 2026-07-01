//! PocketIC coverage for ADR 0034 Slice 19: `SEARCH ... WHERE` bounded heterogeneous
//! equality/range disjunction.
//!
//! Semantics under test:
//!   E_i = property_index_equal(Document, property_i, value_i)
//!   R_j = property_index_numeric_range(Document, property_j, OP_j, value_j)
//!   C   = UNION_i E_i UNION UNION_j R_j  (bounded by MAX_SEARCH_FILTER_DISJUNCTION_ARMS, currently 8)
//!   result = vector_top_k(document_embedding, subjects = C, limit)
//!
//! - Candidate membership is the label-scoped union of postings from equality and range arms before
//!   vector ranking.
//! - A vertex matching any arm is included once, even if it matches several arms.
//! - The Router enforces the 2..=8 syntactic arm bound, proves an active index per arm, normalizes
//!   equality sources and range intervals before any Property Index call, and executes the union
//!   through the shared bounded candidate collector.
//! - Equality and range sources may target the same property; they are not merged with each other.

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

const EMBEDDING_NAME: &str = "adr0034_doc_vec_heterogeneous_disjunction";
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

fn setup_search_where_heterogeneous_disjunction_env(env: &FederationEnv, vector: Principal) {
    register_vector_index(env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(env, vector);

    let doc_label_id = admin_intern_vertex_label(env, "Document");
    let cat_id = admin_intern_property(env, "cat_id");
    let price_id = admin_intern_property(env, "price");
    create_vertex_property_index(
        env,
        "document_cat_id_idx_heterogeneous",
        "Document",
        "cat_id",
        "create_document_cat_id_idx_heterogeneous",
    );
    create_vertex_property_index(
        env,
        "document_price_idx_heterogeneous",
        "Document",
        "price",
        "create_document_price_idx_heterogeneous",
    );

    // doc_eq matches only the equality arm; doc_range only the range arm; doc_both both arms;
    // doc_nonmember is the nearest vector neighbor but matches neither arm.
    let doc_eq = e2e_insert_vertex_with_label_and_two_properties(
        env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        1,
        price_id.raw(),
        100,
    );
    let doc_range = e2e_insert_vertex_with_label_and_two_properties(
        env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        2,
        price_id.raw(),
        5,
    );
    let doc_both = e2e_insert_vertex_with_label_and_two_properties(
        env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        1,
        price_id.raw(),
        5,
    );
    let doc_nonmember = e2e_insert_vertex_with_label_and_two_properties(
        env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        2,
        price_id.raw(),
        100,
    );

    seed_embedding(env, vector, env.graph_source, doc_eq.local_vertex_id, 1.0);
    seed_embedding(
        env,
        vector,
        env.graph_source,
        doc_range.local_vertex_id,
        2.0,
    );
    seed_embedding(env, vector, env.graph_source, doc_both.local_vertex_id, 3.0);
    seed_embedding(
        env,
        vector,
        env.graph_source,
        doc_nonmember.local_vertex_id,
        0.0,
    );
}

fn distance_for_vec_value(value: f32) -> f64 {
    value.powi(2) as f64 * DIMS as f64
}

#[test]
fn search_where_heterogeneous_disjunction_unions_equality_and_range_arms() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_heterogeneous_disjunction_env(&env, vector);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 1 OR d.price < 10 \
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
        "union of equality and range arms must return three rows"
    );

    let rows = extract_rows(result);
    assert_eq!(rows.len(), 3);
    let distances: Vec<f64> = rows
        .iter()
        .map(|r| match r.get("distance").expect("distance column") {
            gleaph_gql_ic::IcWireValue::Float64(d) => *d,
            other => panic!("distance must be Float64, got {other:?}"),
        })
        .collect();
    assert!(
        distances
            .iter()
            .any(|d| (d - distance_for_vec_value(1.0)).abs() < 1e-6),
        "equality-only document must appear"
    );
    assert!(
        distances
            .iter()
            .any(|d| (d - distance_for_vec_value(2.0)).abs() < 1e-6),
        "range-only document must appear"
    );
    assert!(
        distances
            .iter()
            .any(|d| (d - distance_for_vec_value(3.0)).abs() < 1e-6),
        "both-arms document must appear"
    );

    // doc_nonmember is the nearest vector neighbor (distance 0). If the filter were ignored it
    // would appear first. Assert the first returned distance is strictly greater than 0, proving
    // the filter removed the nonmember from the candidate set.
    assert!(
        distances[0] > 0.0,
        "filter must exclude the vector-nearest non-matching document"
    );
}

#[test]
fn search_where_heterogeneous_disjunction_dedupes_both_arm_document() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_heterogeneous_disjunction_env(&env, vector);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 1 OR d.price < 10 \
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
        gleaph_gql_ic::IcWireValue::Int64(v) => assert_eq!(
            *v, 3,
            "document matching both arms must appear exactly once"
        ),
        other => panic!("count must be Int64 3, got {other:?}"),
    }
}

#[test]
fn search_where_heterogeneous_disjunction_parameterized_and_reversed_operands() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_heterogeneous_disjunction_env(&env, vector);

    // Reversed equality operand and parameterized range bound.
    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE 1 = d.cat_id OR $max_price > d.price \
           LIMIT 10 \
         ) DISTANCE AS distance \
         RETURN count(*) AS n"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(vec![
        ("query".to_string(), Value::Bytes(vec_bytes(0.0))),
        ("max_price".to_string(), Value::Int64(10)),
    ])
    .expect("encode params");

    let result = gql_query_with_params_as_admin(&env, &query, params);
    assert_eq!(result.row_count, 1, "aggregate must return one row");
    let rows = extract_rows(result);
    match rows[0].get("n").expect("count column") {
        gleaph_gql_ic::IcWireValue::Int64(v) => assert_eq!(*v, 3, "count must be three"),
        other => panic!("count must be Int64 3, got {other:?}"),
    }
}

#[test]
fn search_where_heterogeneous_disjunction_rejects_missing_range_index() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let doc_label_id = admin_intern_vertex_label(&env, "Document");
    let cat_id = admin_intern_property(&env, "cat_id");
    let price_id = admin_intern_property(&env, "price");
    // Only cat_id has an active index; price does not.
    create_vertex_property_index(
        &env,
        "document_cat_id_idx_missing_range",
        "Document",
        "cat_id",
        "create_document_cat_id_idx_missing_range",
    );

    let doc = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id.raw(),
        cat_id.raw(),
        1,
        price_id.raw(),
        5,
    );
    seed_embedding(&env, vector, env.graph_source, doc.local_vertex_id, 1.0);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 1 OR d.price < 10 \
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
        .expect_err("heterogeneous OR with missing price index must fail");
    assert!(
        err.to_string().contains("active vertex property index"),
        "missing range index must fail with coverage error, got {err}"
    );
}

fn setup_search_where_eight_way_heterogeneous_env(env: &FederationEnv, vector: Principal) {
    register_vector_index(env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(env, vector);

    let doc_label_id = admin_intern_vertex_label(env, "Document");

    // Use eight independent properties so every one of the eight disjunction arms matches
    // exactly one document. Omitting any arm must therefore drop exactly one result.
    const EQ_PROPS: [&str; 4] = ["eq0", "eq1", "eq2", "eq3"];
    const LO_PROPS: [&str; 4] = ["lo0", "lo1", "lo2", "lo3"];

    let eq_ids: Vec<u32> = EQ_PROPS
        .iter()
        .map(|name| admin_intern_property(env, name).raw())
        .collect();
    let lo_ids: Vec<u32> = LO_PROPS
        .iter()
        .map(|name| admin_intern_property(env, name).raw())
        .collect();

    for name in EQ_PROPS {
        create_vertex_property_index(
            env,
            &format!("document_{name}_idx_eight_way_heterogeneous"),
            "Document",
            name,
            &format!("create_document_{name}_idx_eight_way_heterogeneous"),
        );
    }
    for name in LO_PROPS {
        create_vertex_property_index(
            env,
            &format!("document_{name}_idx_eight_way_heterogeneous"),
            "Document",
            name,
            &format!("create_document_{name}_idx_eight_way_heterogeneous"),
        );
    }

    // Create eight documents; document i is matched only by arm i.
    for i in 0..8 {
        let doc = e2e_insert_vertex_with_label(env, env.graph_source, doc_label_id.raw());
        for (j, &id) in eq_ids.iter().enumerate() {
            let value = if i == j { 1 } else { 0 };
            e2e_set_vertex_property(env, env.graph_source, doc.local_vertex_id, id, value);
        }
        for (j, &id) in lo_ids.iter().enumerate() {
            let value = if i == j + 4 { 1 } else { 100 };
            e2e_set_vertex_property(env, env.graph_source, doc.local_vertex_id, id, value);
        }
        seed_embedding(
            env,
            vector,
            env.graph_source,
            doc.local_vertex_id,
            i as f32 + 1.0,
        );
    }
}

#[test]
fn search_where_eight_way_heterogeneous_disjunction_unions_all_values() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_eight_way_heterogeneous_env(&env, vector);

    let arms: Vec<String> = (0..8)
        .map(|i| {
            if i < 4 {
                format!("d.eq{i} = 1")
            } else {
                format!("d.lo{} < 2", i - 4)
            }
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
        "eight-way heterogeneous disjunction must return all eight documents"
    );

    // Prove each arm is independently observable: dropping any one arm must remove exactly one
    // document from the result set.
    for omitted in 0..8 {
        let subset: Vec<String> = (0..8)
            .filter(|&i| i != omitted)
            .map(|i| {
                if i < 4 {
                    format!("d.eq{i} = 1")
                } else {
                    format!("d.lo{} < 2", i - 4)
                }
            })
            .collect();
        let where_clause = subset.join(" OR ");
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
            result.row_count, 7,
            "omitting arm {omitted} must return exactly seven documents"
        );
    }
}

#[test]
fn search_where_nine_arm_heterogeneous_disjunction_is_rejected() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_eight_way_heterogeneous_env(&env, vector);

    let mut arms: Vec<String> = (0..4).map(|i| format!("d.eq{i} = 1")).collect();
    arms.extend((0..4).map(|i| format!("d.lo{i} < 2")));
    arms.push("d.eq0 = 1".to_string());
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
        .expect_err("nine-arm heterogeneous disjunction must be rejected");
    assert!(
        err.to_string().contains("at most 8")
            || err.to_string().contains("disjunction supports at most"),
        "nine arms must fail with an explicit arm-count error, got {err}"
    );
}

#[test]
fn search_where_heterogeneous_disjunction_empty_result_aggregate() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    setup_search_where_heterogeneous_disjunction_env(&env, vector);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 999 OR d.price < 0 \
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
fn non_leading_search_where_heterogeneous_disjunction_unions_values() {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = admin_intern_vertex_label(&env, "Author").raw();
    let doc_label_id = admin_intern_vertex_label(&env, "Document").raw();
    let wrote_label_id = admin_intern_edge_label(&env, "WROTE").raw();
    let cat_id = admin_intern_property(&env, "cat_id");
    let price_id = admin_intern_property(&env, "price");

    create_vertex_property_index(
        &env,
        "document_cat_id_idx_non_leading_heterogeneous",
        "Document",
        "cat_id",
        "create_document_cat_id_idx_non_leading_heterogeneous",
    );
    create_vertex_property_index(
        &env,
        "document_price_idx_non_leading_heterogeneous",
        "Document",
        "price",
        "create_document_price_idx_non_leading_heterogeneous",
    );

    let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let a2 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);

    let d_match_eq = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        cat_id.raw(),
        1,
        price_id.raw(),
        100,
    );
    let d_match_range = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        cat_id.raw(),
        2,
        price_id.raw(),
        5,
    );
    let d_other = e2e_insert_vertex_with_label_and_two_properties(
        &env,
        env.graph_source,
        doc_label_id,
        cat_id.raw(),
        2,
        price_id.raw(),
        100,
    );

    for a in [a1.local_vertex_id, a2.local_vertex_id] {
        e2e_insert_edge_with_label(
            &env,
            env.graph_source,
            a,
            d_match_eq.local_vertex_id,
            wrote_label_id,
        );
        e2e_insert_edge_with_label(
            &env,
            env.graph_source,
            a,
            d_match_range.local_vertex_id,
            wrote_label_id,
        );
        e2e_insert_edge_with_label(
            &env,
            env.graph_source,
            a,
            d_other.local_vertex_id,
            wrote_label_id,
        );
    }

    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_match_eq.local_vertex_id,
        1.0,
    );
    seed_embedding(
        &env,
        vector,
        env.graph_source,
        d_match_range.local_vertex_id,
        2.0,
    );
    seed_embedding(&env, vector, env.graph_source, d_other.local_vertex_id, 0.0);

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.cat_id = 1 OR d.price < 10 \
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
            "only documents matching cat_id=1 or price<10 may appear, got distance {distance}"
        );
    }
}
