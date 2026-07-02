//! PocketIC coverage for ADR 0034 Slice 17 and Slice 18: `SEARCH ... WHERE` bounded numeric range
//! disjunction on the same binding. Slice 17 restricts all arms to the same property; Slice 18
//! permits arms to target different properties. In both cases the Router resolves each arm,
//! proves an active property index per property, derives a finite half-open encoded interval per
//! arm, drops empty intervals, merges overlapping/touching intervals **within each property id**,
//! and executes the union through `lookup_range_page`.
//!
//! Semantics under test:
//!   R_i = property_index_numeric_range(Document, property_i, OP_i, value_i)
//!   R_p   = merge_encoded_intervals({ R_i | property(R_i) = p }) for each property p
//!   R   = UNION_p R_p
//!   C   = label_filter(R)
//!   result = vector_top_k(document_embedding, subjects = C, limit)
//!
//! - Candidate membership is the label-scoped union of postings from each range arm before vector ranking.
//! - A vertex matching any arm is included once, even if overlapping intervals would cover it twice.
//! - The Router enforces the two-to-eight arm bound and merges overlapping/touching encoded intervals per property.
//! - An arm whose resolved interval is empty or contradictory is silently dropped from the union.
//! - Cross-property arms are not merged with each other because encoded numeric keys are property-specific.
//!
//! Test architecture:
//! - Each `#[test]` below builds one fresh federation + vector-index topology and runs a family of
//!   named, sequentially observable cases against it. No PocketIC environment is shared across
//!   `#[test]` functions.
//! - The 13 original `install_federation()` calls are reduced to 5 fixture-family bootstraps
//!   (one per `#[test]` below) while keeping every contract boundary, adversary, and failure
//!   diagnostic independently diagnosable.
//!
//! Former test name -> retained named scenario:
//! - `search_where_cross_property_range_disjunction_unions_values` -> `cross_property_unions_values`
//! - `search_where_cross_property_range_disjunction_rejects_missing_index` -> `cross_property_rejects_missing_score_index`
//! - `search_where_range_disjunction_unions_two_intervals` -> `unions_two_intervals`
//! - `search_where_range_disjunction_dedupes_overlapping_intervals` -> `dedupes_overlapping_intervals`
//! - `search_where_range_disjunction_merges_touching_intervals` -> `merges_touching_intervals`
//! - `search_where_range_disjunction_excludes_gap` -> `excludes_gap`
//! - `search_where_range_disjunction_with_unmatched_arm` -> `with_unmatched_arm`
//! - `search_where_eight_way_range_disjunction_unions_all_values` -> `eight_arm_boundary_and_union_normalization`
//! - `search_where_nine_arm_range_disjunction_is_rejected` -> `nine_arm_is_rejected`
//! - `search_where_range_disjunction_rejects_missing_index` -> `same_property_rejects_missing_price_index`
//! - `search_where_range_disjunction_with_parameterized_predicates` -> `with_parameterized_predicates`
//! - `search_where_range_disjunction_empty_candidate_set_aggregate` -> `empty_candidate_set_aggregate`
//! - `non_leading_search_where_range_disjunction_unions_values` -> `non_leading_unions_values`

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

const EMBEDDING_NAME: &str = "adr0034_doc_vec_range_disjunction";
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

fn install_vector_search_env() -> (FederationEnv, Principal) {
    let env = install_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);
    (env, vector)
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

fn distance_for_vec_value(value: f32) -> f64 {
    f64::from(value).powi(2) * DIMS as f64
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

fn assert_distance_at(
    row: &std::collections::BTreeMap<String, gleaph_gql_ic::IcWireValue>,
    case: &str,
) -> f64 {
    match row
        .get("distance")
        .unwrap_or_else(|| panic!("{case}: distance column"))
    {
        gleaph_gql_ic::IcWireValue::Float64(d) => *d,
        other => panic!("{case}: distance must be Float64, got {other:?}"),
    }
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

fn assert_distance_set(case: &str, result: GqlQueryResult, expected: &[f64]) {
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        expected.len(),
        "{case}: row count mismatch (expected {})",
        expected.len()
    );
    let distances: Vec<f64> = rows.iter().map(|r| assert_distance_at(r, case)).collect();
    for expected_distance in expected {
        assert!(
            distances
                .iter()
                .any(|d| (d - expected_distance).abs() < 1e-6),
            "{case}: expected distance {expected_distance} not found in {distances:?}"
        );
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
// Family A: same-property range disjunction (leading)
// ---------------------------------------------------------------------------

struct SamePropertyPriceFixture {
    env: FederationEnv,
    vector: Principal,
    doc_label: u16,
    price: u32,
}

impl SamePropertyPriceFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let price = admin_intern_property(&env, "price").raw();
        create_vertex_property_index(
            &env,
            "document_price_idx_same_property",
            "Document",
            "price",
            "create_document_price_idx_same_property",
        );
        Self {
            env,
            vector,
            doc_label,
            price,
        }
    }

    fn insert_doc(&self, price_value: i64, embedding_value: f32) -> u32 {
        let id = e2e_insert_vertex_with_label_and_property(
            &self.env,
            self.env.graph_source,
            self.doc_label,
            self.price,
            price_value,
        )
        .local_vertex_id;
        seed_embedding(
            &self.env,
            self.vector,
            self.env.graph_source,
            id,
            embedding_value,
        );
        id
    }

    fn query_ok(&self, query: &str, params: Vec<u8>) -> GqlQueryResult {
        gql_query_with_params_as_admin(&self.env, query, params)
    }
}

#[test]
fn search_where_same_property_range_disjunction_scenarios() {
    let fx = SamePropertyPriceFixture::new();

    // Four documents with deterministic prices and vectors so ranking is predictable.
    fx.insert_doc(5, 0.5);
    fx.insert_doc(15, 1.5);
    fx.insert_doc(25, 2.5);
    fx.insert_doc(35, 3.5);

    run_case("unions_two_intervals", || {
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
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_set(
            "unions_two_intervals",
            result,
            &[distance_for_vec_value(0.5), distance_for_vec_value(3.5)],
        );
    });

    run_case("dedupes_overlapping_intervals", || {
        // Overlapping arms: price >= 10 OR price >= 20 both select price=15, 25, 35.
        // The union must return each document exactly once.
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
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_set(
            "dedupes_overlapping_intervals",
            result,
            &[
                distance_for_vec_value(1.5),
                distance_for_vec_value(2.5),
                distance_for_vec_value(3.5),
            ],
        );
    });

    run_case("merges_touching_intervals", || {
        // Two touching one-sided range arms: price < 10 selects price=5; price >= 10 selects price=15, 25, and 35.
        // The merged interval covers the whole numeric domain, so all four documents survive.
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price < 10 OR d.price >= 10 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN ELEMENT_ID(d), distance \
             ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_set(
            "merges_touching_intervals",
            result,
            &[
                distance_for_vec_value(0.5),
                distance_for_vec_value(1.5),
                distance_for_vec_value(2.5),
                distance_for_vec_value(3.5),
            ],
        );
    });

    run_case("excludes_gap", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price < 10 OR d.price >= 30 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN count(*) AS n"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_aggregate_count("excludes_gap", result, 2);
    });

    run_case("with_unmatched_arm", || {
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
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_aggregate_count("with_unmatched_arm", result, 1);
    });

    run_case("with_parameterized_predicates", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price < $low OR d.price >= $high \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN count(*) AS n"
        );
        let params = gql_params(QUERY_VEC, &[("low", 10), ("high", 30)]);
        let result = fx.query_ok(&query, params);
        assert_aggregate_count("with_parameterized_predicates", result, 2);
    });

    run_case("empty_candidate_set_aggregate", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price < 0 OR d.price >= 100 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN count(*) AS n"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_aggregate_count("empty_candidate_set_aggregate", result, 0);
    });
}

// ---------------------------------------------------------------------------
// Family B: cross-property range disjunction
// ---------------------------------------------------------------------------

struct CrossPropertyFixture {
    env: FederationEnv,
    vector: Principal,
    doc_label: u16,
    price: u32,
    score: u32,
}

impl CrossPropertyFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let price = admin_intern_property(&env, "price").raw();
        let score = admin_intern_property(&env, "score").raw();
        create_vertex_property_index(
            &env,
            "document_price_idx_cross_property",
            "Document",
            "price",
            "create_document_price_idx_cross_property",
        );
        create_vertex_property_index(
            &env,
            "document_score_idx_cross_property",
            "Document",
            "score",
            "create_document_score_idx_cross_property",
        );
        Self {
            env,
            vector,
            doc_label,
            price,
            score,
        }
    }

    fn insert_doc(&self, price_value: i64, score_value: i64, embedding_value: f32) -> u32 {
        let id = e2e_insert_vertex_with_label_and_two_properties(
            &self.env,
            self.env.graph_source,
            self.doc_label,
            self.price,
            price_value,
            self.score,
            score_value,
        )
        .local_vertex_id;
        seed_embedding(
            &self.env,
            self.vector,
            self.env.graph_source,
            id,
            embedding_value,
        );
        id
    }

    fn query_ok(&self, query: &str, params: Vec<u8>) -> GqlQueryResult {
        gql_query_with_params_as_admin(&self.env, query, params)
    }
}

#[test]
fn search_where_cross_property_range_disjunction_scenarios() {
    let fx = CrossPropertyFixture::new();

    // Documents:
    // - price_doc matches only the price arm (price < 10, score out of range).
    // - score_doc matches only the score arm (score >= 4, price out of range).
    // - both_doc matches both arms.
    // - neither_doc matches neither.
    fx.insert_doc(5, 1, 0.5);
    fx.insert_doc(50, 5, 1.5);
    fx.insert_doc(5, 5, 2.5);
    fx.insert_doc(50, 1, 3.5);

    run_case("cross_property_unions_values", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price < 10 OR d.score >= 4 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN ELEMENT_ID(d), distance \
             ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_set(
            "cross_property_unions_values",
            result,
            &[
                distance_for_vec_value(0.5),
                distance_for_vec_value(1.5),
                distance_for_vec_value(2.5),
            ],
        );
    });
}

// ---------------------------------------------------------------------------
// Family C: 8/9-arm boundary
// ---------------------------------------------------------------------------

struct EightArmBoundaryFixture {
    env: FederationEnv,
    vector: Principal,
    doc_label: u16,
    price: u32,
}

impl EightArmBoundaryFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let price = admin_intern_property(&env, "price").raw();
        create_vertex_property_index(
            &env,
            "document_price_idx_eight_way",
            "Document",
            "price",
            "create_document_price_idx_eight_way",
        );
        Self {
            env,
            vector,
            doc_label,
            price,
        }
    }

    fn insert_doc(&self, price_value: i64, embedding_value: f32) -> u32 {
        let id = e2e_insert_vertex_with_label_and_property(
            &self.env,
            self.env.graph_source,
            self.doc_label,
            self.price,
            price_value,
        )
        .local_vertex_id;
        seed_embedding(
            &self.env,
            self.vector,
            self.env.graph_source,
            id,
            embedding_value,
        );
        id
    }

    fn query_ok(&self, query: &str, params: Vec<u8>) -> GqlQueryResult {
        gql_query_with_params_as_admin(&self.env, query, params)
    }

    fn query_result(&self, query: &str, params: Vec<u8>) -> Result<GqlQueryResult, RouterError> {
        gql_query_with_params_as_admin_result(&self.env, query, params)
    }
}

#[test]
fn search_where_eight_way_range_disjunction_scenarios() {
    let fx = EightArmBoundaryFixture::new();

    // Eight documents with price values 0, 10, ..., 70 and deterministic vectors.
    for i in 0..8 {
        fx.insert_doc(i as i64 * 10, i as f32 + 1.0);
    }

    run_case("eight_arm_boundary_and_union_normalization", || {
        // Eight same-property lower-bound arms. The Router normalizes them to one encoded
        // interval covering the whole domain, so this proves the 8-arm acceptance boundary and
        // the normalized union path, not that every arm independently contributes candidates.
        let arms: Vec<String> = (0..8).map(|i| format!("d.price >= {}", i * 10)).collect();
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
        assert_distance_set(
            "eight_arm_boundary_and_union_normalization",
            result,
            &expected,
        );
    });

    run_case("nine_arm_is_rejected", || {
        let arms: Vec<String> = (0..9).map(|i| format!("d.price >= {}", i * 10)).collect();
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
        assert_rejected_with("nine_arm_is_rejected", result, "at most");
    });
}

// ---------------------------------------------------------------------------
// Family D: non-leading range disjunction
// ---------------------------------------------------------------------------

struct NonLeadingRangeFixture {
    env: FederationEnv,
    vector: Principal,
    author_label: u16,
    doc_label: u16,
    wrote_label: u16,
    price: u32,
}

impl NonLeadingRangeFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let author_label = admin_intern_vertex_label(&env, "Author").raw();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let wrote_label = admin_intern_edge_label(&env, "WROTE").raw();
        let price = admin_intern_property(&env, "price").raw();
        create_vertex_property_index(
            &env,
            "document_price_idx_non_leading",
            "Document",
            "price",
            "create_document_price_idx_non_leading",
        );
        Self {
            env,
            vector,
            author_label,
            doc_label,
            wrote_label,
            price,
        }
    }

    fn insert_author(&self) -> u32 {
        e2e_insert_vertex_with_label(&self.env, self.env.graph_source, self.author_label)
            .local_vertex_id
    }

    fn insert_doc(&self, price_value: i64, embedding_value: f32) -> u32 {
        let id = e2e_insert_vertex_with_label_and_property(
            &self.env,
            self.env.graph_source,
            self.doc_label,
            self.price,
            price_value,
        )
        .local_vertex_id;
        seed_embedding(
            &self.env,
            self.vector,
            self.env.graph_source,
            id,
            embedding_value,
        );
        id
    }

    fn connect(&self, author: u32, doc: u32) {
        e2e_insert_edge_with_label(
            &self.env,
            self.env.graph_source,
            author,
            doc,
            self.wrote_label,
        );
    }

    fn query_ok(&self, query: &str, params: Vec<u8>) -> GqlQueryResult {
        gql_query_with_params_as_admin(&self.env, query, params)
    }
}

#[test]
fn search_where_non_leading_range_disjunction_scenarios() {
    let fx = NonLeadingRangeFixture::new();
    let a1 = fx.insert_author();
    let a2 = fx.insert_author();

    // d_low matches only the lower arm (price < 10), d_high only the upper arm (price >= 20),
    // and d_gap matches neither.
    let d_low = fx.insert_doc(5, 1.0);
    let d_high = fx.insert_doc(25, 2.0);
    let d_gap = fx.insert_doc(15, 0.0);

    for a in [a1, a2] {
        fx.connect(a, d_low);
        fx.connect(a, d_high);
        fx.connect(a, d_gap);
    }

    run_case("non_leading_unions_values", || {
        let query = format!(
            "MATCH (a:Author)-[:WROTE]->(d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price < 10 OR d.price >= 20 \
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
            let distance = assert_distance_at(row, "non_leading_unions_values");
            assert!(
                (distance - distance_for_vec_value(1.0)).abs() < 1e-6
                    || (distance - distance_for_vec_value(2.0)).abs() < 1e-6,
                "non_leading_unions_values: only documents with price < 10 or price >= 20 may appear, got distance {distance}"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Family E: missing-index rejections
// ---------------------------------------------------------------------------

#[test]
fn search_where_range_disjunction_missing_index_rejections() {
    let (env, vector) = install_vector_search_env();
    let _ = vector;
    let doc_label = admin_intern_vertex_label(&env, "Document").raw();
    let price = admin_intern_property(&env, "price").raw();
    let score = admin_intern_property(&env, "score").raw();

    run_case("cross_property_rejects_missing_score_index", || {
        create_vertex_property_index(
            &env,
            "document_price_idx_missing_cross",
            "Document",
            "price",
            "create_document_price_idx_missing_cross",
        );
        let doc = e2e_insert_vertex_with_label_and_two_properties(
            &env,
            env.graph_source,
            doc_label,
            price,
            5,
            score,
            5,
        );
        seed_embedding(&env, vector, env.graph_source, doc.local_vertex_id, 1.0);

        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price < 10 OR d.score >= 4 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN ELEMENT_ID(d), distance"
        );
        let result =
            gql_query_with_params_as_admin_result(&env, &query, gql_params(QUERY_VEC, &[]));
        assert_rejected_with(
            "cross_property_rejects_missing_score_index",
            result,
            "active vertex property index",
        );
    });

    run_case("same_property_rejects_missing_price_index", || {
        // Drop the price index created above so the same-property query now lacks an active index.
        drop_vertex_property_index(
            &env,
            "document_price_idx_missing_cross",
            true,
            "drop_document_price_idx_missing_cross",
        );
        let doc =
            e2e_insert_vertex_with_label_and_property(&env, env.graph_source, doc_label, price, 5);
        seed_embedding(&env, vector, env.graph_source, doc.local_vertex_id, 1.0);

        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price < 10 OR d.price >= 20 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN ELEMENT_ID(d), distance"
        );
        let result =
            gql_query_with_params_as_admin_result(&env, &query, gql_params(QUERY_VEC, &[]));
        assert_rejected_with(
            "same_property_rejects_missing_price_index",
            result,
            "active vertex property index",
        );
    });
}
