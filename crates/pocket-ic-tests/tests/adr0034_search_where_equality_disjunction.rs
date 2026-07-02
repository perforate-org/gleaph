//! PocketIC coverage for ADR 0034 Slice 15 and Slice 16: `SEARCH ... WHERE` equality disjunction.
//!
//! Semantics under test:
//!   C_i = property_index_equal(Document, property_i, value_i)
//!   C   = UNION_i C_i  (bounded by MAX_SEARCH_FILTER_DISJUNCTION_ARMS, currently 8)
//!   result = vector_top_k(document_embedding, subjects = C, limit)
//!
//! - Candidate membership is the label-scoped union of postings before vector ranking.
//! - A vertex matching any arm is included once, even if it matches several values or appears in
//!   multiple arms.
//! - The Router enforces the two-to-eight arm bound; the planner accepts any length.
//! - Slice 16 extends the same contract to arms that reference distinct properties, provided each
//!   property has an active vertex property index.
//!
//! Test architecture:
//! - Each `#[test]` below builds one fresh federation + vector-index topology and runs a family of
//!   named, sequentially observable cases against it. No PocketIC environment is shared across
//!   `#[test]` functions.
//! - The 10 original `install_federation()` calls are reduced to 4 fixture-family bootstraps while
//!   keeping every contract boundary, adversary, and failure diagnostic independently diagnosable.
//!
//! Former test names -> retained scenario families:
//! - search_where_equality_disjunction_unions_two_values          -> Family A: unions_two_values
//! - search_where_equality_disjunction_excludes_unmatched_value   -> Family A: excludes_unmatched_value
//! - search_where_equality_disjunction_dedupes_duplicate_arm      -> Family A: dedupes_duplicate_arm
//! - search_where_equality_disjunction_parameter_predicates       -> Family A: parameter_predicates
//! - search_where_equality_disjunction_empty_result_aggregate   -> Family A: empty_result_aggregate
//! - search_where_eight_way_disjunction_unions_all_values         -> Family B: eight_arm_boundary_and_unique_matches
//! - search_where_nine_arm_equality_disjunction_is_rejected     -> Family B: nine_arm_is_rejected
//! - search_where_equality_disjunction_across_properties_returns_union -> Family C: cross_property_unions_values
//! - search_where_equality_disjunction_rejects_missing_exact_index -> Family C: rejects_missing_exact_index
//! - non_leading_search_where_equality_disjunction_unions_values -> Family D: non_leading_unions_values

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
    admin_intern_vertex_label, create_vertex_property_index, drop_vertex_property_index,
    e2e_insert_edge_with_label, e2e_insert_vertex_with_label,
    e2e_insert_vertex_with_label_and_property, e2e_insert_vertex_with_label_and_two_properties,
    gql_query_with_params_as_admin, install_federation, install_vector_canister,
};
use gleaph_router::types::{AdminAttachVectorIndexShardArgs, RegisterVectorIndexArgs};
use std::panic::AssertUnwindSafe;

const EMBEDDING_NAME: &str = "adr0034_doc_vec_equality_disjunction";
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

fn gql_params(query_value: f32, extra: &[(&str, i64)]) -> Vec<u8> {
    let mut items = vec![("query".to_string(), Value::Bytes(vec_bytes(query_value)))];
    for (name, value) in extra {
        items.push((name.to_string(), Value::Int64(*value)));
    }
    gleaph_gql_ic::wire::encode_gql_params_blob(items).expect("encode params")
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

fn assert_distance_rows(
    case: &str,
    result: GqlQueryResult,
    expected_count: usize,
    expected_distance: f64,
) {
    let rows = assert_rows(case, result, expected_count);
    for row in &rows {
        let d = assert_distance_at(case, row);
        assert!(
            (d - expected_distance).abs() < 1e-6,
            "{case}: distance mismatch: {d}"
        );
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
    let err = result.expect_err(&format!("{case}: expected error result"));
    let message = err.to_string();
    assert!(
        message.contains(needle),
        "{case}: expected error containing `{needle}`, got `{message}`"
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
// Family A: same-property leading equality disjunction
// ---------------------------------------------------------------------------

struct SamePropertyEqualityFixture {
    env: FederationEnv,
}

impl SamePropertyEqualityFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let cat_id = admin_intern_property(&env, "cat_id").raw();
        create_vertex_property_index(
            &env,
            "document_cat_id_idx",
            "Document",
            "cat_id",
            "create_document_cat_id_idx_disjunction",
        );

        // Three documents with distinct cat_id values and deterministic vectors.
        for (cat, vec) in [(1, 1.0), (2, 2.0), (3, 3.0)] {
            let doc = e2e_insert_vertex_with_label_and_property(
                &env,
                env.graph_source,
                doc_label,
                cat_id,
                cat,
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
fn search_where_same_property_equality_disjunction_scenarios() {
    let fx = SamePropertyEqualityFixture::new();

    run_case("unions_two_values", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.cat_id = 1 OR d.cat_id = 2 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN ELEMENT_ID(d), distance \
             ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_set(
            "unions_two_values",
            result,
            &[distance_for_vec_value(1.0), distance_for_vec_value(2.0)],
        );
    });

    run_case("excludes_unmatched_value", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.cat_id = 999 OR d.cat_id = 1 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN ELEMENT_ID(d), distance \
             ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_rows(
            "excludes_unmatched_value",
            result,
            1,
            distance_for_vec_value(1.0),
        );
    });

    run_case("dedupes_duplicate_arm", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.cat_id = 1 OR d.cat_id = 1 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN ELEMENT_ID(d), distance \
             ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_rows(
            "dedupes_duplicate_arm",
            result,
            1,
            distance_for_vec_value(1.0),
        );
    });

    run_case("parameter_predicates", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.cat_id = $a OR d.cat_id = $b \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN ELEMENT_ID(d), distance \
             ORDER BY distance ASC"
        );
        let params = gql_params(QUERY_VEC, &[("a", 1), ("b", 2)]);
        let result = fx.query_ok(&query, params);
        assert_distance_set(
            "parameter_predicates",
            result,
            &[distance_for_vec_value(1.0), distance_for_vec_value(2.0)],
        );
    });

    run_case("empty_result_aggregate", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.cat_id = 999 OR d.cat_id = 998 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN count(*) AS n"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_aggregate_count("empty_result_aggregate", result, 0);
    });
}

// ---------------------------------------------------------------------------
// Family B: 8/9-arm boundary for equality disjunction
// ---------------------------------------------------------------------------

struct EightArmBoundaryFixture {
    env: FederationEnv,
}

impl EightArmBoundaryFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let cat_id = admin_intern_property(&env, "cat_id").raw();
        create_vertex_property_index(
            &env,
            "document_cat_id_idx_eight_way",
            "Document",
            "cat_id",
            "create_document_cat_id_idx_eight_way",
        );

        // Eight documents, each with a distinct cat_id and a deterministic vector.
        // Every arm selects a different document, so omitting any arm removes its unique result.
        for i in 0..8 {
            let doc = e2e_insert_vertex_with_label_and_property(
                &env,
                env.graph_source,
                doc_label,
                cat_id,
                i as i64,
            );
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

#[test]
fn search_where_eight_way_equality_disjunction_scenarios() {
    let fx = EightArmBoundaryFixture::new();

    run_case("eight_arm_boundary_and_unique_matches", || {
        let arms: Vec<String> = (0..8).map(|i| format!("d.cat_id = {i}")).collect();
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
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        let expected: Vec<f64> = (0..8)
            .map(|i| distance_for_vec_value(i as f32 + 1.0))
            .collect();
        assert_distance_set("eight_arm_boundary_and_unique_matches", result, &expected);
    });

    run_case("nine_arm_is_rejected", || {
        let arms: Vec<String> = (0..9).map(|i| format!("d.cat_id = {i}")).collect();
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
        let result = fx.query_result(&query, gql_params(QUERY_VEC, &[]));
        assert_rejected_with(
            "nine_arm_is_rejected",
            result,
            "disjunction supports at most",
        );
    });
}

// ---------------------------------------------------------------------------
// Family C: cross-property union and missing-index rejection
// ---------------------------------------------------------------------------

struct CrossPropertyEqualityFixture {
    env: FederationEnv,
    vector: Principal,
    doc_label: u16,
    cat_id: u32,
    tenant_id: u32,
}

impl CrossPropertyEqualityFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let cat_id = admin_intern_property(&env, "cat_id").raw();
        let tenant_id = admin_intern_property(&env, "tenant_id").raw();
        create_vertex_property_index(
            &env,
            "document_cat_id_idx_cross",
            "Document",
            "cat_id",
            "create_document_cat_id_idx_cross",
        );
        create_vertex_property_index(
            &env,
            "document_tenant_id_idx_cross",
            "Document",
            "tenant_id",
            "create_document_tenant_id_idx_cross",
        );

        // doc_a matches only cat_id=1; doc_b only tenant_id=2; doc_c both; doc_d neither.
        // doc_d is seeded at distance 0.0 and would dominate top-k if the filter were ignored.
        let fixtures = [
            (1_i64, 0_i64, 1.0_f32), // doc_a
            (0, 2, 2.0),             // doc_b
            (1, 2, 3.0),             // doc_c
            (0, 0, 0.0),             // doc_d
        ];
        for (cat, tenant, vec) in fixtures {
            let doc = e2e_insert_vertex_with_label_and_two_properties(
                &env,
                env.graph_source,
                doc_label,
                cat_id,
                cat,
                tenant_id,
                tenant,
            );
            seed_embedding(&env, vector, env.graph_source, doc.local_vertex_id, vec);
        }

        Self {
            env,
            vector,
            doc_label,
            cat_id,
            tenant_id,
        }
    }

    fn query_ok(&self, query: &str, params: Vec<u8>) -> GqlQueryResult {
        gql_query_with_params_as_admin(&self.env, query, params)
    }

    fn query_result(&self, query: &str, params: Vec<u8>) -> Result<GqlQueryResult, RouterError> {
        gql_query_with_params_as_admin_result(&self.env, query, params)
    }
}

#[test]
fn search_where_cross_property_equality_disjunction_scenarios() {
    let fx = CrossPropertyEqualityFixture::new();

    run_case("cross_property_unions_values", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.cat_id = 1 OR d.tenant_id = 2 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN ELEMENT_ID(d), distance \
             ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        let rows = assert_distance_set(
            "cross_property_unions_values",
            result,
            &[
                distance_for_vec_value(1.0),
                distance_for_vec_value(2.0),
                distance_for_vec_value(3.0),
            ],
        );
        // doc_d is the globally nearest non-matching vertex; if the filter were ignored it would
        // appear first at distance 0.0. The first returned distance must be strictly positive.
        let first_distance = assert_distance_at("cross_property_unions_values", &rows[0]);
        assert!(
            first_distance > 0.0,
            "cross_property_unions_values: filter must exclude the vector-nearest non-matching document"
        );
    });

    run_case("rejects_missing_exact_index", || {
        drop_vertex_property_index(
            &fx.env,
            "document_tenant_id_idx_cross",
            true,
            "drop_document_tenant_id_idx_cross",
        );

        // Insert a document that would match the cat_id arm but needs the missing tenant index.
        let doc = e2e_insert_vertex_with_label_and_two_properties(
            &fx.env,
            fx.env.graph_source,
            fx.doc_label,
            fx.cat_id,
            1,
            fx.tenant_id,
            1,
        );
        seed_embedding(
            &fx.env,
            fx.vector,
            fx.env.graph_source,
            doc.local_vertex_id,
            1.0,
        );

        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.cat_id = 1 OR d.tenant_id = 1 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN ELEMENT_ID(d), distance"
        );
        let result = fx.query_result(&query, gql_params(QUERY_VEC, &[]));
        assert_rejected_with(
            "rejects_missing_exact_index",
            result,
            "requires an active vertex property index",
        );
    });
}

// ---------------------------------------------------------------------------
// Family D: non-leading equality disjunction
// ---------------------------------------------------------------------------

struct NonLeadingEqualityFixture {
    env: FederationEnv,
}

impl NonLeadingEqualityFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let author_label = admin_intern_vertex_label(&env, "Author").raw();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let wrote_label = admin_intern_edge_label(&env, "WROTE").raw();
        let cat_id = admin_intern_property(&env, "cat_id").raw();
        let tenant_id = admin_intern_property(&env, "tenant_id").raw();
        create_vertex_property_index(
            &env,
            "document_cat_id_idx_non_leading",
            "Document",
            "cat_id",
            "create_document_cat_id_idx_non_leading",
        );
        create_vertex_property_index(
            &env,
            "document_tenant_id_idx_non_leading",
            "Document",
            "tenant_id",
            "create_document_tenant_id_idx_non_leading",
        );

        let a1 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label).local_vertex_id;
        let a2 = e2e_insert_vertex_with_label(&env, env.graph_source, author_label).local_vertex_id;

        // d_match_a matches only cat_id=1; d_match_b matches only tenant_id=2; d_other matches neither.
        let docs = [
            (1_i64, 1_i64, 1.0_f32), // d_match_a
            (3, 2, 2.0),             // d_match_b
            (3, 3, 0.0),             // d_other
        ];
        let doc_ids: Vec<u32> = docs
            .iter()
            .map(|(cat, tenant, _vec)| {
                e2e_insert_vertex_with_label_and_two_properties(
                    &env,
                    env.graph_source,
                    doc_label,
                    cat_id,
                    *cat,
                    tenant_id,
                    *tenant,
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
fn search_where_non_leading_equality_disjunction_scenarios() {
    let fx = NonLeadingEqualityFixture::new();

    run_case("non_leading_unions_values", || {
        let query = format!(
            "MATCH (a:Author)-[:WROTE]->(d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.cat_id = 1 OR d.tenant_id = 2 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN a, d, distance ORDER BY distance ASC"
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
                "non_leading_unions_values: only documents matching cat_id=1 or tenant_id=2 may appear, got distance {distance}"
            );
        }
    });
}
