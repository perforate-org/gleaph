//! PocketIC coverage for ADR 0034 Slices 9, 10, 11, 12 and 14: `SEARCH ... WHERE` numeric range,
//! two-sided numeric range, mixed equality-plus-one-sided-range, mixed equality-plus-two-sided-range,
//! and bounded N-way equality (1..=8) plus a single one- or two-sided range.
//!
//! Semantics under test:
//!   R = property_index_numeric_range(Document, price, OP, value)
//!   result = vector_top_k(document_embedding, subjects = R, limit)
//!
//! - Candidate membership is the exact label-scoped numeric range (optionally intersected with one or
//!   more equality sieves) before vector ranking.
//! - A globally nearer vertex with an out-of-range or non-numeric value, with a value outside a two-sided
//!   interval, or with the wrong equality value(s), must not consume a top-k position.
//!
//! Test architecture:
//! - Each `#[test]` below builds one fresh federation + vector-index topology and runs a family of named,
//!   sequentially observable cases against it. No PocketIC environment is shared across `#[test]` functions.
//! - The 28 original `install_federation()` calls are reduced to 8 fixture-family bootstraps
//!   (one per `#[test]` below) while keeping every contract boundary, adversary, and failure diagnostic
//!   independently diagnosable.

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
    e2e_insert_vertex_with_label_and_two_properties, e2e_set_vertex_property,
    gql_query_with_params_as_admin, install_federation, install_vector_canister,
};
use gleaph_router::types::{AdminAttachVectorIndexShardArgs, RegisterVectorIndexArgs};
use std::panic::AssertUnwindSafe;

const EMBEDDING_NAME: &str = "adr0034_doc_vec_range";
const INDEX_ID: u32 = 1;
const DIMS: u16 = 16;
const QUERY_VEC: f32 = 5.0;
const FAR_VEC: f32 = 10.0;
const NEAR_VEC: f32 = 5.0;

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

fn assert_distance_rows(
    case: &str,
    result: GqlQueryResult,
    expected_count: usize,
    expected_distance: f64,
) {
    let rows = assert_rows(case, result, expected_count);
    for row in &rows {
        match row
            .get("distance")
            .unwrap_or_else(|| panic!("{case}: distance column"))
        {
            gleaph_gql_ic::IcWireValue::Float64(d) => {
                assert!(
                    (d - expected_distance).abs() < 1e-6,
                    "{case}: distance mismatch: {d}"
                );
            }
            other => panic!("{case}: distance must be Float64, got {other:?}"),
        }
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
// Family A: non-leading one-sided and two-sided numeric range
// ---------------------------------------------------------------------------

struct EdgeRangeFixture {
    env: FederationEnv,
    vector: Principal,
    author_label: u16,
    doc_label: u16,
    wrote_label: u16,
    price: u32,
}

impl EdgeRangeFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let author_label = admin_intern_vertex_label(&env, "Author").raw();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let wrote_label = admin_intern_edge_label(&env, "WROTE").raw();
        let price = admin_intern_property(&env, "price").raw();
        create_vertex_property_index(
            &env,
            "document_price_idx",
            "Document",
            "price",
            "create_document_price_idx",
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

    fn insert_doc_with_price(&self, price_value: i64, embedding_value: f32) -> u32 {
        let id = e2e_insert_vertex_with_label_and_property(
            &self.env,
            self.env.graph_source,
            self.doc_label,
            self.price,
            price_value,
        )
        .local_vertex_id;
        self.seed(id, embedding_value);
        id
    }

    fn insert_doc_no_price(&self, embedding_value: f32) -> u32 {
        let id = e2e_insert_vertex_with_label(&self.env, self.env.graph_source, self.doc_label)
            .local_vertex_id;
        self.seed(id, embedding_value);
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

    fn seed(&self, vertex_id: u32, value: f32) {
        seed_embedding(
            &self.env,
            self.vector,
            self.env.graph_source,
            vertex_id,
            value,
        );
    }

    fn query_ok(&self, query: &str, params: Vec<u8>) -> GqlQueryResult {
        gql_query_with_params_as_admin(&self.env, query, params)
    }
}

#[test]
fn search_where_numeric_range_non_leading_scenarios() {
    let fx = EdgeRangeFixture::new();
    let a1 = fx.insert_author();
    let a2 = fx.insert_author();

    // d_match is the only in-range document with a far embedding.
    let d_match = fx.insert_doc_with_price(7, FAR_VEC);
    // Global near-miss documents must be excluded by the numeric range.
    let d_near = fx.insert_doc_with_price(3, NEAR_VEC);
    let d_too_cheap = fx.insert_doc_with_price(4, NEAR_VEC);
    let d_low = fx.insert_doc_with_price(2, NEAR_VEC);
    // d_none has no price posting and must not enter the range.
    let d_none = fx.insert_doc_no_price(6.0);

    fx.connect(a1, d_match);
    fx.connect(a1, d_near);
    fx.connect(a1, d_too_cheap);
    fx.connect(a1, d_low);
    fx.connect(a1, d_none);

    run_case("excludes_out_of_range_and_missing_property", || {
        let query = format!(
            "MATCH (a:Author)-[:WROTE]->(d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price >= 5 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN a, d, distance ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_rows(
            "excludes_out_of_range_and_missing_property",
            result,
            1,
            400.0,
        );
    });

    run_case("parameter_predicate", || {
        let query = format!(
            "MATCH (a:Author)-[:WROTE]->(d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price > $min_price \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN a, d, distance ORDER BY distance ASC"
        );
        let params = gql_params(QUERY_VEC, &[("min_price", 5)]);
        let result = fx.query_ok(&query, params);
        assert_distance_rows("parameter_predicate", result, 1, 400.0);
    });

    run_case("empty_aggregate", || {
        let query = format!(
            "MATCH (a:Author)-[:WROTE]->(d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price >= 10 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN count(*) AS n"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_aggregate_count("empty_aggregate", result, 0);
    });

    run_case("two_sided_excludes_out_of_range", || {
        let query = format!(
            "MATCH (a:Author)-[:WROTE]->(d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price >= 5 AND d.price < 10 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN a, d, distance ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_rows("two_sided_excludes_out_of_range", result, 1, 400.0);
    });

    run_case("two_sided_empty_intersection", || {
        let query = format!(
            "MATCH (a:Author)-[:WROTE]->(d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price > 10 AND d.price <= 10 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN count(*) AS n"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_aggregate_count("two_sided_empty_intersection", result, 0);
    });
    run_case("multiplicity", || {
        fx.connect(a2, d_match);

        let query = format!(
            "MATCH (a:Author)-[:WROTE]->(d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price >= 5 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN a, d, distance ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        // One global top-k hit joined against two prefix rows produces two output rows.
        assert_rows("multiplicity", result, 2);
    });
}

// ---------------------------------------------------------------------------
// Family B: leading one-sided and two-sided numeric range
// ---------------------------------------------------------------------------

struct LeadingPriceFixture {
    env: FederationEnv,
    vector: Principal,
    doc_label: u16,
    price: u32,
}

impl LeadingPriceFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let price = admin_intern_property(&env, "price").raw();
        create_vertex_property_index(
            &env,
            "document_price_idx",
            "Document",
            "price",
            "create_document_price_idx",
        );
        Self {
            env,
            vector,
            doc_label,
            price,
        }
    }

    fn insert_doc_with_price(&self, price_value: i64, embedding_value: f32) -> u32 {
        let id = e2e_insert_vertex_with_label_and_property(
            &self.env,
            self.env.graph_source,
            self.doc_label,
            self.price,
            price_value,
        )
        .local_vertex_id;
        self.seed(id, embedding_value);
        id
    }

    fn seed(&self, vertex_id: u32, value: f32) {
        seed_embedding(
            &self.env,
            self.vector,
            self.env.graph_source,
            vertex_id,
            value,
        );
    }

    fn query_ok(&self, query: &str, params: Vec<u8>) -> GqlQueryResult {
        gql_query_with_params_as_admin(&self.env, query, params)
    }
}

#[test]
fn search_where_numeric_range_leading_scenarios() {
    let fx = LeadingPriceFixture::new();

    // Operator exactness must be independent of distance. Seed both candidates identically.
    let _d_at = fx.insert_doc_with_price(5, FAR_VEC);
    let _d_above = fx.insert_doc_with_price(6, FAR_VEC);

    run_case("operators_are_exact", || {
        let params = gql_params(QUERY_VEC, &[]);
        for (op, expected) in [(">=", 2), (">", 1), ("<=", 1), ("<", 0)] {
            let query = format!(
                "MATCH (d:Document) \
                 SEARCH d IN ( \
                   VECTOR INDEX {EMBEDDING_NAME} FOR $query \
                   WHERE d.price {op} 5 \
                   LIMIT 2 \
                 ) DISTANCE AS distance \
                 RETURN d, distance ORDER BY distance ASC"
            );
            let result = fx.query_ok(&query, params.clone());
            assert_rows("operators_are_exact", result, expected);
        }
    });

    // Add global near-misses and boundary documents for the remaining leading cases.
    let d_near = fx.insert_doc_with_price(3, NEAR_VEC);
    let d_match = fx.insert_doc_with_price(7, FAR_VEC);
    let d_10 = fx.insert_doc_with_price(10, FAR_VEC);

    run_case("excludes_out_of_range", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price >= 5 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN d, distance ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_rows("excludes_out_of_range", result, 1, 400.0);
        // d_near is globally nearer but out of range and must not win.
        let _ = d_near;
    });

    run_case("two_sided_excludes_out_of_range", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price >= 5 AND d.price < 10 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN d, distance ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_rows("two_sided_excludes_out_of_range", result, 1, 400.0);
    });

    run_case("endpoint_strictness", || {
        let params = gql_params(QUERY_VEC, &[]);
        for (lower_op, upper_op, expected) in [
            (">=", "<", 3),
            (">=", "<=", 4),
            (">", "<", 2),
            (">", "<=", 3),
        ] {
            let query = format!(
                "MATCH (d:Document) \
                 SEARCH d IN ( \
                   VECTOR INDEX {EMBEDDING_NAME} FOR $query \
                   WHERE d.price {lower_op} 5 AND d.price {upper_op} 10 \
                   LIMIT 10 \
                 ) DISTANCE AS distance \
                 RETURN d, distance ORDER BY distance ASC"
            );
            let result = fx.query_ok(&query, params.clone());
            assert_rows(
                &format!("endpoint_strictness_{lower_op}_{upper_op}"),
                result,
                expected,
            );
        }
    });

    let _d_4 = fx.insert_doc_with_price(4, FAR_VEC);
    let _d_6b = fx.insert_doc_with_price(6, FAR_VEC);

    run_case("equal_endpoint", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price >= 5 AND d.price <= 5 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN count(*) AS n"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_aggregate_count("equal_endpoint", result, 1);
    });

    run_case("reversed_operands", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE 10 > d.price AND d.price >= 5 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN d, distance ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_rows("reversed_operands", result, 1, 400.0);
    });

    // d_10 is exactly at the upper boundary and must be excluded by the half-open interval.
    let _ = d_10;
    let _ = d_match;
}

// ---------------------------------------------------------------------------
// Family C: mixed equality + one-sided range
// ---------------------------------------------------------------------------

struct MixedEqRangeFixture {
    env: FederationEnv,
    vector: Principal,
    author_label: u16,
    doc_label: u16,
    wrote_label: u16,
    category: u32,
    price: u32,
}

impl MixedEqRangeFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let author_label = admin_intern_vertex_label(&env, "Author").raw();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let wrote_label = admin_intern_edge_label(&env, "WROTE").raw();
        let category = admin_intern_property(&env, "category").raw();
        let price = admin_intern_property(&env, "price").raw();
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
        Self {
            env,
            vector,
            author_label,
            doc_label,
            wrote_label,
            category,
            price,
        }
    }

    fn insert_author(&self) -> u32 {
        e2e_insert_vertex_with_label(&self.env, self.env.graph_source, self.author_label)
            .local_vertex_id
    }

    fn insert_doc_with_two_props(
        &self,
        category_value: i64,
        price_value: i64,
        embedding_value: f32,
    ) -> u32 {
        let id = e2e_insert_vertex_with_label_and_two_properties(
            &self.env,
            self.env.graph_source,
            self.doc_label,
            self.category,
            category_value,
            self.price,
            price_value,
        )
        .local_vertex_id;
        self.seed(id, embedding_value);
        id
    }

    fn seed(&self, vertex_id: u32, value: f32) {
        seed_embedding(
            &self.env,
            self.vector,
            self.env.graph_source,
            vertex_id,
            value,
        );
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
fn search_where_mixed_equality_and_range_scenarios() {
    let fx = MixedEqRangeFixture::new();
    let a1 = fx.insert_author();

    let d_match = fx.insert_doc_with_two_props(1, 7, FAR_VEC);
    let d_eq_only = fx.insert_doc_with_two_props(1, 3, NEAR_VEC);
    let d_range_only = fx.insert_doc_with_two_props(2, 7, NEAR_VEC);
    let d_empty = fx.insert_doc_with_two_props(3, 3, FAR_VEC);

    fx.connect(a1, d_match);
    fx.connect(a1, d_eq_only);
    fx.connect(a1, d_range_only);

    run_case("excludes_single_arm_matches", || {
        let query = format!(
            "MATCH (a:Author)-[:WROTE]->(d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.category = 1 AND d.price >= 5 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN a, d, distance ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_rows("excludes_single_arm_matches", result, 1, 400.0);
    });

    run_case("parameter_reversed_order", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE $min_price <= d.price AND $category = d.category \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN d, distance ORDER BY distance ASC"
        );
        let params = gql_params(QUERY_VEC, &[("min_price", 5), ("category", 1)]);
        let result = fx.query_ok(&query, params);
        assert_distance_rows("parameter_reversed_order", result, 1, 400.0);
    });

    run_case("empty_aggregate", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.category = 3 AND d.price >= 5 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN count(*) AS n"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_aggregate_count("empty_aggregate", result, 0);
    });

    // d_empty documents an alternate category whose equality arm is not satisfied.
    let _ = d_empty;
}

// ---------------------------------------------------------------------------
// Family D: mixed equality + two-sided range
// ---------------------------------------------------------------------------

struct MixedEqTwoSidedFixture {
    env: FederationEnv,
    vector: Principal,
    author_label: u16,
    doc_label: u16,
    wrote_label: u16,
    category: u32,
    price: u32,
}

impl MixedEqTwoSidedFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let author_label = admin_intern_vertex_label(&env, "Author").raw();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let wrote_label = admin_intern_edge_label(&env, "WROTE").raw();
        let category = admin_intern_property(&env, "category").raw();
        let price = admin_intern_property(&env, "price").raw();
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
        Self {
            env,
            vector,
            author_label,
            doc_label,
            wrote_label,
            category,
            price,
        }
    }

    fn insert_author(&self) -> u32 {
        e2e_insert_vertex_with_label(&self.env, self.env.graph_source, self.author_label)
            .local_vertex_id
    }

    fn insert_doc_with_two_props(
        &self,
        category_value: i64,
        price_value: i64,
        embedding_value: f32,
    ) -> u32 {
        let id = e2e_insert_vertex_with_label_and_two_properties(
            &self.env,
            self.env.graph_source,
            self.doc_label,
            self.category,
            category_value,
            self.price,
            price_value,
        )
        .local_vertex_id;
        self.seed(id, embedding_value);
        id
    }

    fn seed(&self, vertex_id: u32, value: f32) {
        seed_embedding(
            &self.env,
            self.vector,
            self.env.graph_source,
            vertex_id,
            value,
        );
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
fn search_where_mixed_equality_plus_two_sided_scenarios() {
    let fx = MixedEqTwoSidedFixture::new();
    let a1 = fx.insert_author();

    let d_match = fx.insert_doc_with_two_props(1, 7, FAR_VEC);
    let d_eq_only = fx.insert_doc_with_two_props(1, 3, NEAR_VEC);
    let d_range_only = fx.insert_doc_with_two_props(2, 7, NEAR_VEC);
    let d_upper_only = fx.insert_doc_with_two_props(1, 12, NEAR_VEC);

    fx.connect(a1, d_match);
    fx.connect(a1, d_eq_only);
    fx.connect(a1, d_range_only);
    fx.connect(a1, d_upper_only);

    run_case("excludes_single_arm_matches", || {
        let query = format!(
            "MATCH (a:Author)-[:WROTE]->(d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.category = 1 AND d.price >= 5 AND d.price < 10 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN a, d, distance ORDER BY distance ASC"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_distance_rows("excludes_single_arm_matches", result, 1, 400.0);
    });

    run_case("non_leading_parameterized", || {
        let query = format!(
            "MATCH (a:Author)-[:WROTE]->(d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE $min_price <= d.price AND $category = d.category AND d.price < $max_price \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN a, d, distance ORDER BY distance ASC"
        );
        let params = gql_params(
            QUERY_VEC,
            &[("min_price", 5), ("category", 1), ("max_price", 10)],
        );
        let result = fx.query_ok(&query, params);
        assert_distance_rows("non_leading_parameterized", result, 1, 400.0);
    });

    run_case("empty_intersection", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.category = 1 AND d.price > 10 AND d.price <= 10 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN count(*) AS n"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_aggregate_count("empty_intersection", result, 0);
    });

    // Add boundary values for endpoint strictness and equal-endpoint cases.
    let _d_5 = fx.insert_doc_with_two_props(1, 5, FAR_VEC);
    let _d_6 = fx.insert_doc_with_two_props(1, 6, FAR_VEC);
    let d_10 = fx.insert_doc_with_two_props(1, 10, FAR_VEC);

    run_case("endpoint_strictness", || {
        let params = gql_params(QUERY_VEC, &[]);
        for (lower_op, upper_op, expected) in [
            (">=", "<", 3),
            (">=", "<=", 4),
            (">", "<", 2),
            (">", "<=", 3),
        ] {
            let query = format!(
                "MATCH (d:Document) \
                 SEARCH d IN ( \
                   VECTOR INDEX {EMBEDDING_NAME} FOR $query \
                   WHERE d.category = 1 AND d.price {lower_op} 5 AND d.price {upper_op} 10 \
                   LIMIT 10 \
                 ) DISTANCE AS distance \
                 RETURN d, distance ORDER BY distance ASC"
            );
            let result = fx.query_ok(&query, params.clone());
            assert_rows(
                &format!("endpoint_strictness_{lower_op}_{upper_op}"),
                result,
                expected,
            );
        }
    });

    let _d_4 = fx.insert_doc_with_two_props(1, 4, NEAR_VEC);
    let _d_6b = fx.insert_doc_with_two_props(1, 6, NEAR_VEC);

    run_case("equal_endpoint", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.category = 1 AND d.price >= 5 AND d.price <= 5 \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN count(*) AS n"
        );
        let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
        assert_aggregate_count("equal_endpoint", result, 1);
    });

    // d_10 is exactly at the upper inclusive/exclusive boundary and must not be returned for (..10).
    let _ = d_10;
}

// ---------------------------------------------------------------------------
// Family E: N-way equality (3 arms) + one-sided range
// ---------------------------------------------------------------------------

struct NwayEqRangeFixture {
    env: FederationEnv,
    vector: Principal,
    author_label: u16,
    doc_label: u16,
    wrote_label: u16,
    category: u32,
    tenant: u32,
    status: u32,
    price: u32,
}

impl NwayEqRangeFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let author_label = admin_intern_vertex_label(&env, "Author").raw();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let wrote_label = admin_intern_edge_label(&env, "WROTE").raw();
        let category = admin_intern_property(&env, "category").raw();
        let tenant = admin_intern_property(&env, "tenant").raw();
        let status = admin_intern_property(&env, "status").raw();
        let price = admin_intern_property(&env, "price").raw();
        for property in ["category", "tenant", "status", "price"] {
            create_vertex_property_index(
                &env,
                &format!("document_{property}_idx"),
                "Document",
                property,
                &format!("create_document_{property}_idx"),
            );
        }
        Self {
            env,
            vector,
            author_label,
            doc_label,
            wrote_label,
            category,
            tenant,
            status,
            price,
        }
    }

    fn insert_author(&self) -> u32 {
        e2e_insert_vertex_with_label(&self.env, self.env.graph_source, self.author_label)
            .local_vertex_id
    }

    fn insert_doc_with_props(&self, values: &[(u32, i64)], embedding_value: f32) -> u32 {
        let id = e2e_insert_vertex_with_label(&self.env, self.env.graph_source, self.doc_label)
            .local_vertex_id;
        for (property_id, value) in values {
            e2e_set_vertex_property(&self.env, self.env.graph_source, id, *property_id, *value);
        }
        self.seed(id, embedding_value);
        id
    }

    fn seed(&self, vertex_id: u32, value: f32) {
        seed_embedding(
            &self.env,
            self.vector,
            self.env.graph_source,
            vertex_id,
            value,
        );
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
fn search_where_mixed_nway_equality_plus_range_excludes_partial_matches() {
    let fx = NwayEqRangeFixture::new();
    let a1 = fx.insert_author();

    let d_match = fx.insert_doc_with_props(
        &[
            (fx.category, 1),
            (fx.tenant, 2),
            (fx.status, 3),
            (fx.price, 7),
        ],
        FAR_VEC,
    );
    let d_bad_category = fx.insert_doc_with_props(
        &[
            (fx.category, 9),
            (fx.tenant, 2),
            (fx.status, 3),
            (fx.price, 7),
        ],
        NEAR_VEC,
    );
    let d_bad_tenant = fx.insert_doc_with_props(
        &[
            (fx.category, 1),
            (fx.tenant, 9),
            (fx.status, 3),
            (fx.price, 7),
        ],
        NEAR_VEC,
    );
    let d_bad_status = fx.insert_doc_with_props(
        &[
            (fx.category, 1),
            (fx.tenant, 2),
            (fx.status, 9),
            (fx.price, 7),
        ],
        NEAR_VEC,
    );
    let d_bad_range = fx.insert_doc_with_props(
        &[
            (fx.category, 1),
            (fx.tenant, 2),
            (fx.status, 3),
            (fx.price, 3),
        ],
        NEAR_VEC,
    );

    fx.connect(a1, d_match);
    fx.connect(a1, d_bad_category);
    fx.connect(a1, d_bad_tenant);
    fx.connect(a1, d_bad_status);
    fx.connect(a1, d_bad_range);

    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.category = 1 AND d.tenant = 2 AND d.status = 3 AND d.price >= 5 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN a, d, distance ORDER BY distance ASC"
    );
    let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
    assert_distance_rows(
        "nway_equality_plus_range_excludes_partial_matches",
        result,
        1,
        400.0,
    );
}

// ---------------------------------------------------------------------------
// Family F: missing-index and invalid-conjunction rejections
// ---------------------------------------------------------------------------

#[test]
fn search_where_numeric_range_missing_index_rejections() {
    let (env, vector) = install_vector_search_env();
    let _ = vector;
    let doc_label = admin_intern_vertex_label(&env, "Document").raw();
    let price = admin_intern_property(&env, "price").raw();
    let _category = admin_intern_property(&env, "category").raw();
    let _score = admin_intern_property(&env, "score").raw();
    let property_names = ["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8"];
    for name in &property_names {
        let _ = admin_intern_property(&env, name);
    }

    run_case("rejects_missing_index", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price >= 1 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN d, distance"
        );
        let result =
            gql_query_with_params_as_admin_result(&env, &query, gql_params(QUERY_VEC, &[]));
        assert_rejected_with(
            "rejects_missing_index",
            result,
            "requires an active vertex property index",
        );
    });

    run_case("nine_equality_arms_plus_range_is_rejected", || {
        create_vertex_property_index(
            &env,
            "document_price_idx",
            "Document",
            "price",
            "create_document_price_idx_nine",
        );
        let filters = property_names
            .iter()
            .enumerate()
            .map(|(i, name)| format!("d.{name} = {i}"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE {filters} AND d.price >= 5 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN d, distance"
        );
        let result =
            gql_query_with_params_as_admin_result(&env, &query, gql_params(QUERY_VEC, &[]));
        assert_rejected_with(
            "nine_equality_arms_plus_range_is_rejected",
            result,
            "at most 8 equality conjuncts",
        );
    });

    run_case("rejects_different_property_conjunction", || {
        let query = format!(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.price >= 5 AND d.score < 100 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN d, distance"
        );
        let result =
            gql_query_with_params_as_admin_result(&env, &query, gql_params(QUERY_VEC, &[]));
        assert_rejected_with(
            "rejects_different_property_conjunction",
            result,
            "same property",
        );
    });

    run_case(
        "mixed_equality_and_range_rejects_missing_category_index",
        || {
            e2e_insert_vertex_with_label_and_property(&env, env.graph_source, doc_label, price, 7);
            let query = format!(
                "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.category = 1 AND d.price >= 5 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN d, distance"
            );
            let result =
                gql_query_with_params_as_admin_result(&env, &query, gql_params(QUERY_VEC, &[]));
            assert_rejected_with(
                "mixed_equality_and_range_rejects_missing_category_index",
                result,
                "active vertex property index",
            );
        },
    );

    run_case(
        "mixed_equality_plus_two_sided_rejects_missing_equality_index",
        || {
            let query = format!(
                "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX {EMBEDDING_NAME} FOR $query \
               WHERE d.category = 1 AND d.price >= 5 AND d.price < 10 \
               LIMIT 1 \
             ) DISTANCE AS distance \
             RETURN d, distance"
            );
            let result =
                gql_query_with_params_as_admin_result(&env, &query, gql_params(QUERY_VEC, &[]));
            assert_rejected_with(
                "mixed_equality_plus_two_sided_rejects_missing_equality_index",
                result,
                "active vertex property index",
            );
        },
    );
}

// ---------------------------------------------------------------------------
// Family F2: missing range index rejection (requires a category-only index fixture)
// ---------------------------------------------------------------------------

#[test]
fn search_where_mixed_equality_plus_two_sided_rejects_missing_range_index() {
    let (env, vector) = install_vector_search_env();
    let _ = vector;
    let doc_label = admin_intern_vertex_label(&env, "Document").raw();
    let category = admin_intern_property(&env, "category").raw();
    let _price = admin_intern_property(&env, "price").raw();

    create_vertex_property_index(
        &env,
        "document_category_idx",
        "Document",
        "category",
        "create_document_category_idx_reject",
    );
    e2e_insert_vertex_with_label_and_property(&env, env.graph_source, doc_label, category, 1);

    let query = format!(
        "MATCH (d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE d.category = 1 AND d.price >= 5 AND d.price < 10 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN d, distance"
    );
    let result = gql_query_with_params_as_admin_result(&env, &query, gql_params(QUERY_VEC, &[]));
    assert_rejected_with(
        "mixed_equality_plus_two_sided_rejects_missing_range_index",
        result,
        "active vertex property index",
    );
}

// ---------------------------------------------------------------------------
// Family G: 8 equality arms + two-sided range boundary
// ---------------------------------------------------------------------------

struct EightArmBoundaryFixture {
    env: FederationEnv,
    vector: Principal,
    author_label: u16,
    doc_label: u16,
    wrote_label: u16,
    price: u32,
    properties: Vec<u32>,
    property_names: &'static [&'static str],
}

impl EightArmBoundaryFixture {
    fn new() -> Self {
        let (env, vector) = install_vector_search_env();
        let author_label = admin_intern_vertex_label(&env, "Author").raw();
        let doc_label = admin_intern_vertex_label(&env, "Document").raw();
        let wrote_label = admin_intern_edge_label(&env, "WROTE").raw();
        let price = admin_intern_property(&env, "price").raw();
        let property_names: &[&str] = &["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7"];
        for name in property_names {
            create_vertex_property_index(
                &env,
                &format!("document_{name}_idx"),
                "Document",
                name,
                &format!("create_document_{name}_idx"),
            );
        }
        create_vertex_property_index(
            &env,
            "document_price_idx",
            "Document",
            "price",
            "create_document_price_idx_eight",
        );
        let properties: Vec<u32> = property_names
            .iter()
            .map(|name| admin_intern_property(&env, name).raw())
            .collect();
        Self {
            env,
            vector,
            author_label,
            doc_label,
            wrote_label,
            price,
            properties,
            property_names,
        }
    }

    fn insert_author(&self) -> u32 {
        e2e_insert_vertex_with_label(&self.env, self.env.graph_source, self.author_label)
            .local_vertex_id
    }

    fn insert_doc_with_props(&self, values: &[(u32, i64)], embedding_value: f32) -> u32 {
        let id = e2e_insert_vertex_with_label(&self.env, self.env.graph_source, self.doc_label)
            .local_vertex_id;
        for (property_id, value) in values {
            e2e_set_vertex_property(&self.env, self.env.graph_source, id, *property_id, *value);
        }
        self.seed(id, embedding_value);
        id
    }

    fn seed(&self, vertex_id: u32, value: f32) {
        seed_embedding(
            &self.env,
            self.vector,
            self.env.graph_source,
            vertex_id,
            value,
        );
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
fn search_where_mixed_eight_equality_arms_plus_two_sided_range_hits_boundary() {
    let fx = EightArmBoundaryFixture::new();
    let a1 = fx.insert_author();

    let match_values: Vec<_> = fx
        .properties
        .iter()
        .enumerate()
        .map(|(i, p)| (*p, i as i64))
        .collect();
    let fail_values: Vec<_> = fx
        .properties
        .iter()
        .enumerate()
        .take(7)
        .map(|(i, p)| (*p, i as i64))
        .collect();

    let d_match = fx.insert_doc_with_props(&match_values, FAR_VEC);
    let d_fail = fx.insert_doc_with_props(&fail_values, NEAR_VEC);
    // d_fail must also satisfy the price range so that only the missing p7 equality excludes it.
    e2e_set_vertex_property(&fx.env, fx.env.graph_source, d_match, fx.price, 7);
    e2e_set_vertex_property(&fx.env, fx.env.graph_source, d_fail, fx.price, 7);

    fx.connect(a1, d_match);
    fx.connect(a1, d_fail);

    let filters = fx
        .property_names
        .iter()
        .enumerate()
        .map(|(i, name)| format!("d.{name} = {i}"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let query = format!(
        "MATCH (a:Author)-[:WROTE]->(d:Document) \
         SEARCH d IN ( \
           VECTOR INDEX {EMBEDDING_NAME} FOR $query \
           WHERE {filters} AND d.price >= 5 AND d.price < 10 \
           LIMIT 1 \
         ) DISTANCE AS distance \
         RETURN a, d, distance ORDER BY distance ASC"
    );
    let result = fx.query_ok(&query, gql_params(QUERY_VEC, &[]));
    assert_distance_rows(
        "nway_equality_plus_range_excludes_partial_matches",
        result,
        1,
        400.0,
    );
}
