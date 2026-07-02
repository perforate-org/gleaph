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
//!
//! Test architecture:
//! - Each `#[test]` below builds one fresh federation + vector-index topology and runs a family of
//!   named, sequentially observable cases against it. No PocketIC environment is shared across
//!   `#[test]` functions.
//! - The 8 original `install_federation()` calls are reduced to 4 fixture-family bootstraps while
//!   keeping every contract boundary, adversary, and failure diagnostic independently diagnosable.
//!
//! Former test names -> retained scenario families:
//! - search_where_heterogeneous_disjunction_unions_equality_and_range_arms          -> Family A: unions_equality_and_range_arms
//! - search_where_heterogeneous_disjunction_dedupes_both_arm_document              -> Family A: dedupes_both_arm_document
//! - search_where_heterogeneous_disjunction_parameterized_and_reversed_operands  -> Family A: parameterized_and_reversed_operands
//! - search_where_heterogeneous_disjunction_empty_result_aggregate                 -> Family A: empty_result_aggregate
//! - search_where_heterogeneous_disjunction_rejects_missing_range_index            -> Family B: rejects_missing_range_index
//! - search_where_eight_way_heterogeneous_disjunction_unions_all_values            -> Family C: eight_arm_boundary_and_independent_sources
//! - search_where_nine_arm_heterogeneous_disjunction_is_rejected                  -> Family C: nine_arm_is_rejected
//! - non_leading_search_where_heterogeneous_disjunction_unions_values             -> Family D: non_leading_unions_values

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
use std::panic::AssertUnwindSafe;

const EMBEDDING_NAME: &str = "adr0034_doc_vec_heterogeneous_disjunction";
const INDEX_ID: u32 = 1;
const DIMS: u16 = 16;
const QUERY_VEC: f32 = 0.0;

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

fn install_vector_search_env() -> (FederationEnv, Principal) {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);
    (env, vector)
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

fn gql_params(query_value: f32, extra: &[(&str, i64)]) -> Vec<u8> {
    let mut items = vec![("query".to_string(), Value::Bytes(vec_bytes(query_value)))];
    for (name, value) in extra {
        items.push((name.to_string(), Value::Int64(*value)));
    }
    gleaph_gql_ic::wire::encode_gql_params_blob(items).expect("encode params")
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

fn distance_for_vec_value(value: f32) -> f64 {
    (value as f64).powi(2) * DIMS as f64
}

fn assert_rows(
    case: &str,
    result: GqlQueryResult,
    expected_count: usize,
) -> Vec<std::collections::BTreeMap<String, gleaph_gql_ic::IcWireValue>> {
    assert_eq!(
        result.row_count, expected_count as u64,
        "{case}: row count mismatch"
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        expected_count,
        "{case}: extracted row count mismatch"
    );
    rows
}

fn assert_distance_at(
    case: &str,
    row: &std::collections::BTreeMap<String, gleaph_gql_ic::IcWireValue>,
) -> f64 {
    match row
        .get("distance")
        .unwrap_or_else(|| panic!("{case}: distance column"))
    {
        gleaph_gql_ic::IcWireValue::Float64(d) => *d,
        other => panic!("{case}: distance must be Float64, got {other:?}"),
    }
}

fn assert_distance_set(
    case: &str,
    result: GqlQueryResult,
    expected: &[f64],
) -> Vec<std::collections::BTreeMap<String, gleaph_gql_ic::IcWireValue>> {
    let rows = assert_rows(case, result, expected.len());
    let mut got: Vec<f64> = rows.iter().map(|r| assert_distance_at(case, r)).collect();
    let mut exp = expected.to_vec();
    got.sort_by(|a, b| a.partial_cmp(b).unwrap());
    exp.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(got, exp, "{case}: distance set mismatch");
    rows
}

fn assert_aggregate_count(case: &str, result: GqlQueryResult, expected: i64) {
    assert_eq!(
        result.row_count, 1,
        "{case}: aggregate must return exactly one row"
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1, "{case}: aggregate row count mismatch");
    match rows[0]
        .get("n")
        .unwrap_or_else(|| panic!("{case}: count column"))
    {
        gleaph_gql_ic::IcWireValue::Int64(v) => {
            assert_eq!(*v, expected, "{case}: count mismatch")
        }
        other => panic!("{case}: count must be Int64, got {other:?}"),
    }
}

fn assert_rejected_with(case: &str, result: Result<GqlQueryResult, RouterError>, needle: &str) {
    let err = result
        .expect_err(&format!("{case}: expected error result"))
        .to_string();
    assert!(
        err.contains(needle),
        "{case}: expected error containing `{needle}`, got `{err}`"
    );
}

fn run_case<F: FnOnce()>(name: &str, body: F) {
    if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(body)) {
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_else(|| format!("case `{name}` panicked"));
        panic!("case `{name}` failed: {message}");
    }
}

// ---------------------------------------------------------------------------
// Family A: common leading heterogeneous equality/range OR
// ---------------------------------------------------------------------------

struct CommonHeterogeneousFixture {
    env: FederationEnv,
}

impl CommonHeterogeneousFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let cat_id = admin_intern_property(&env, "cat_id").raw();
        let price_id = admin_intern_property(&env, "price").raw();
        create_vertex_property_index(
            &env,
            "document_cat_id_idx_heterogeneous",
            "Document",
            "cat_id",
            "create_document_cat_id_idx_heterogeneous",
        );
        create_vertex_property_index(
            &env,
            "document_price_idx_heterogeneous",
            "Document",
            "price",
            "create_document_price_idx_heterogeneous",
        );

        // doc_eq matches only the equality arm; doc_range only the range arm; doc_both both arms;
        // doc_nonmember is the nearest vector neighbor but matches neither arm.
        let fixtures = [
            (1_i64, 100_i64, 1.0_f32), // doc_eq
            (2, 5, 2.0),               // doc_range
            (1, 5, 3.0),               // doc_both
            (2, 100, 0.0),             // doc_nonmember
        ];
        for (cat, price, vec) in fixtures {
            let doc = e2e_insert_vertex_with_label_and_two_properties(
                &env,
                env.graph_source,
                doc_label,
                cat_id,
                cat,
                price_id,
                price,
            );
            seed_embedding(&env, vector, env.graph_source, doc.local_vertex_id, vec);
        }

        Self { env }
    }

    fn query_ok(&self, query: &str, params: Vec<u8>) -> GqlQueryResult {
        gql_query_with_params_as_admin(&self.env, query, params)
    }
}

#[test]
fn search_where_heterogeneous_disjunction_common_scenarios() {
    let fx = CommonHeterogeneousFixture::new();

    run_case("unions_equality_and_range_arms", || {
        let query = format!(
            "MATCH (d:Document) SEARCH d IN ( VECTOR INDEX {EMBEDDING_NAME} FOR $query WHERE d.cat_id = 1 OR d.price < 10 LIMIT 10 ) DISTANCE AS distance RETURN ELEMENT_ID(d), distance ORDER BY distance ASC"
        );
        let rows = assert_distance_set(
            "unions_equality_and_range_arms",
            fx.query_ok(&query, gql_params(QUERY_VEC, &[])),
            &[
                distance_for_vec_value(1.0),
                distance_for_vec_value(2.0),
                distance_for_vec_value(3.0),
            ],
        );
        let first_distance = assert_distance_at("unions_equality_and_range_arms", &rows[0]);
        assert!(
            first_distance > 0.0,
            "unions_equality_and_range_arms: filter must exclude the vector-nearest non-matching document"
        );
    });

    run_case("dedupes_both_arm_document", || {
        let query = format!(
            "MATCH (d:Document) SEARCH d IN ( VECTOR INDEX {EMBEDDING_NAME} FOR $query WHERE d.cat_id = 1 OR d.price < 10 LIMIT 10 ) DISTANCE AS distance RETURN count(*) AS n"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_aggregate_count("dedupes_both_arm_document", result, 3);
    });

    run_case("parameterized_and_reversed_operands", || {
        let query = format!(
            "MATCH (d:Document) SEARCH d IN ( VECTOR INDEX {EMBEDDING_NAME} FOR $query WHERE 1 = d.cat_id OR $max_price > d.price LIMIT 10 ) DISTANCE AS distance RETURN count(*) AS n"
        );
        let params = gql_params(QUERY_VEC, &[("max_price", 10)]);
        let result = fx.query_ok(&query, params);
        assert_aggregate_count("parameterized_and_reversed_operands", result, 3);
    });

    run_case("empty_result_aggregate", || {
        let query = format!(
            "MATCH (d:Document) SEARCH d IN ( VECTOR INDEX {EMBEDDING_NAME} FOR $query WHERE d.cat_id = 999 OR d.price < 0 LIMIT 10 ) DISTANCE AS distance RETURN count(*) AS n"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_aggregate_count("empty_result_aggregate", result, 0);
    });
}

// ---------------------------------------------------------------------------
// Family B: missing range index rejection
// ---------------------------------------------------------------------------

struct MissingRangeIndexFixture {
    env: FederationEnv,
}

impl MissingRangeIndexFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let cat_id = admin_intern_property(&env, "cat_id").raw();
        let price_id = admin_intern_property(&env, "price").raw();
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
            doc_label,
            cat_id,
            1,
            price_id,
            5,
        );
        seed_embedding(&env, vector, env.graph_source, doc.local_vertex_id, 1.0);

        Self { env }
    }
}

#[test]
fn search_where_heterogeneous_disjunction_missing_index_scenarios() {
    let fx = MissingRangeIndexFixture::new();

    run_case("rejects_missing_range_index", || {
        let query = format!(
            "MATCH (d:Document) SEARCH d IN ( VECTOR INDEX {EMBEDDING_NAME} FOR $query WHERE d.cat_id = 1 OR d.price < 10 LIMIT 10 ) DISTANCE AS distance RETURN ELEMENT_ID(d), distance"
        );
        let result =
            gql_query_with_params_as_admin_result(&fx.env, &query, gql_params(QUERY_VEC, &[]));
        assert_rejected_with(
            "rejects_missing_range_index",
            result,
            "active vertex property index",
        );
    });
}

// ---------------------------------------------------------------------------
// Family C: 8/9-arm heterogeneous boundary
// ---------------------------------------------------------------------------

struct EightArmBoundaryFixture {
    env: FederationEnv,
}

impl EightArmBoundaryFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();

        // Use eight independent properties so every one of the eight disjunction arms matches
        // exactly one document. Omitting any arm must therefore drop exactly one result.
        const EQ_PROPS: [&str; 4] = ["eq0", "eq1", "eq2", "eq3"];
        const LO_PROPS: [&str; 4] = ["lo0", "lo1", "lo2", "lo3"];

        let eq_ids: Vec<u32> = EQ_PROPS
            .iter()
            .map(|name| admin_intern_property(&env, name).raw())
            .collect();
        let lo_ids: Vec<u32> = LO_PROPS
            .iter()
            .map(|name| admin_intern_property(&env, name).raw())
            .collect();

        for name in EQ_PROPS {
            create_vertex_property_index(
                &env,
                &format!("document_{name}_idx_eight_way_heterogeneous"),
                "Document",
                name,
                &format!("create_document_{name}_idx_eight_way_heterogeneous"),
            );
        }
        for name in LO_PROPS {
            create_vertex_property_index(
                &env,
                &format!("document_{name}_idx_eight_way_heterogeneous"),
                "Document",
                name,
                &format!("create_document_{name}_idx_eight_way_heterogeneous"),
            );
        }

        // Create eight documents; document i is matched only by arm i.
        for i in 0..8 {
            let doc = e2e_insert_vertex_with_label(&env, env.graph_source, doc_label);
            for (j, &id) in eq_ids.iter().enumerate() {
                let value = if i == j { 1 } else { 0 };
                e2e_set_vertex_property(&env, env.graph_source, doc.local_vertex_id, id, value);
            }
            for (j, &id) in lo_ids.iter().enumerate() {
                let value = if i == j + 4 { 1 } else { 100 };
                e2e_set_vertex_property(&env, env.graph_source, doc.local_vertex_id, id, value);
            }
            seed_embedding(
                &env,
                vector,
                env.graph_source,
                doc.local_vertex_id,
                i as f32 + 1.0,
            );
        }

        Self { env }
    }

    fn query_ok(&self, query: &str, params: Vec<u8>) -> GqlQueryResult {
        gql_query_with_params_as_admin(&self.env, query, params)
    }

    fn query_result(&self, query: &str, params: Vec<u8>) -> Result<GqlQueryResult, RouterError> {
        gql_query_with_params_as_admin_result(&self.env, query, params)
    }
}

fn eight_arm_where_clause(excluded: &[usize]) -> String {
    let mut arms: Vec<String> = Vec::with_capacity(8 - excluded.len());
    for i in 0..8 {
        if excluded.contains(&i) {
            continue;
        }
        if i < 4 {
            arms.push(format!("d.eq{i} = 1"));
        } else {
            arms.push(format!("d.lo{} < 2", i - 4));
        }
    }
    arms.join(" OR ")
}

#[test]
fn search_where_heterogeneous_disjunction_eight_arm_scenarios() {
    let fx = EightArmBoundaryFixture::new();

    run_case("eight_arm_boundary_and_independent_sources", || {
        let where_clause = eight_arm_where_clause(&[]);
        let query = format!(
            "MATCH (d:Document) SEARCH d IN ( VECTOR INDEX {EMBEDDING_NAME} FOR $query WHERE {where_clause} LIMIT 10 ) DISTANCE AS distance RETURN ELEMENT_ID(d), distance ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        let expected: Vec<f64> = (0..8)
            .map(|i| distance_for_vec_value(i as f32 + 1.0))
            .collect();
        assert_distance_set(
            "eight_arm_boundary_and_independent_sources",
            result,
            &expected,
        );

        // Prove each arm is independently observable: dropping any one arm must remove exactly
        // its unique matching document from the result set.
        for omitted in 0..8 {
            let case = format!("eight_way_omits_arm_{omitted}");
            let where_clause = eight_arm_where_clause(&[omitted]);
            let query = format!(
                "MATCH (d:Document) SEARCH d IN ( VECTOR INDEX {EMBEDDING_NAME} FOR $query WHERE {where_clause} LIMIT 10 ) DISTANCE AS distance RETURN ELEMENT_ID(d), distance ORDER BY distance ASC"
            );
            let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
            let rows = assert_rows(&case, result, 7);
            let omitted_distance = distance_for_vec_value(omitted as f32 + 1.0);
            for row in &rows {
                let d = assert_distance_at(&case, row);
                assert!(
                    (d - omitted_distance).abs() >= 1e-6,
                    "{case}: omitting arm {omitted} must exclude its matching document (distance {omitted_distance}), got {d}"
                );
            }
        }
    });

    run_case("nine_arm_is_rejected", || {
        let mut arms: Vec<String> = (0..4).map(|i| format!("d.eq{i} = 1")).collect();
        arms.extend((0..4).map(|i| format!("d.lo{i} < 2")));
        arms.push("d.eq0 = 1".to_string());
        let where_clause = arms.join(" OR ");
        let query = format!(
            "MATCH (d:Document) SEARCH d IN ( VECTOR INDEX {EMBEDDING_NAME} FOR $query WHERE {where_clause} LIMIT 10 ) DISTANCE AS distance RETURN ELEMENT_ID(d), distance"
        );
        let result = fx.query_result(&query, gql_params(QUERY_VEC, &[]));
        assert_rejected_with(
            "nine_arm_is_rejected",
            result,
            "disjunction supports at most",
        );
    });
}

// ---------------------------------------------------------------------------
// Family D: non-leading heterogeneous equality/range OR
// ---------------------------------------------------------------------------

struct NonLeadingHeterogeneousFixture {
    env: FederationEnv,
}

impl NonLeadingHeterogeneousFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let author_label = admin_intern_vertex_label(&env, "Author").raw();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let wrote_label = admin_intern_edge_label(&env, "WROTE").raw();
        let cat_id = admin_intern_property(&env, "cat_id").raw();
        let price_id = admin_intern_property(&env, "price").raw();

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

        let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label).local_vertex_id;
        let a2 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label).local_vertex_id;

        // d_match_eq matches only cat_id=1; d_match_range matches only price<10; d_other matches neither.
        let docs = [
            (1_i64, 100_i64, 1.0_f32), // d_match_eq
            (2, 5, 2.0),               // d_match_range
            (2, 100, 0.0),             // d_other
        ];
        let doc_ids: Vec<u32> = docs
            .iter()
            .map(|(cat, price, _vec)| {
                e2e_insert_vertex_with_label_and_two_properties(
                    &env,
                    env.graph_source,
                    doc_label,
                    cat_id,
                    *cat,
                    price_id,
                    *price,
                )
                .local_vertex_id
            })
            .collect();

        for author in [a1, a2] {
            for doc_id in &doc_ids {
                e2e_insert_edge_with_label(&env, env.graph_source, author, *doc_id, wrote_label);
            }
        }

        for (idx, (_, _, vec)) in docs.iter().enumerate() {
            seed_embedding(&env, vector, env.graph_source, doc_ids[idx], *vec);
        }

        Self { env }
    }

    fn query_ok(&self, query: &str, params: Vec<u8>) -> GqlQueryResult {
        gql_query_with_params_as_admin(&self.env, query, params)
    }
}

#[test]
fn search_where_non_leading_heterogeneous_disjunction_scenarios() {
    let fx = NonLeadingHeterogeneousFixture::new();

    run_case("non_leading_unions_values", || {
        let query = format!(
            "MATCH (a:Author)-[:WROTE]->(d:Document) SEARCH d IN ( VECTOR INDEX {EMBEDDING_NAME} FOR $query WHERE d.cat_id = 1 OR d.price < 10 LIMIT 10 ) DISTANCE AS distance RETURN a, d, distance ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_eq!(
            result.row_count, 4,
            "non_leading_unions_values: two authors joined to two surviving documents produce four rows"
        );
        let rows = extract_rows(result);
        assert_eq!(rows.len(), 4);
        for row in &rows {
            let distance = assert_distance_at("non_leading_unions_values", row);
            assert!(
                (distance - distance_for_vec_value(1.0)).abs() < 1e-6
                    || (distance - distance_for_vec_value(2.0)).abs() < 1e-6,
                "non_leading_unions_values: only documents matching cat_id=1 or price<10 may appear, got distance {distance}"
            );
        }
    });
}
