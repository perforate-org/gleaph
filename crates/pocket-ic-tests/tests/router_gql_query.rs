//! PocketIC: router `gql_query` composite path (parse → plan → graph dispatch).
//!
//! Covers single-shard NodeScan, index-seeded property equality, `ELEMENT_ID` rows, and
//! multi-shard router index lookup with per-shard `seed_bindings_blob` fan-out.

use gleaph_gql::Value;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, GlobalVertexId};
use gleaph_graph_kernel::path::{GraphPathEdgeId, GraphPathVertexId};
use gleaph_pocket_ic_tests::{
    DEST_SHARD, SOURCE_SHARD, admin_intern_edge_label, admin_intern_property,
    create_directed_edge_property_index, create_edge_property_index,
    create_undirected_edge_property_index, create_vertex_property_index,
    drop_vertex_property_index, e2e_insert_directed_edge_with_property,
    e2e_insert_undirected_edge_with_property, e2e_insert_vertex, e2e_insert_vertex_with_property,
    e2e_insert_vertex_with_two_properties, gql_execute_idempotent_as_admin,
    gql_execute_idempotent_as_admin_expect_err, gql_query_as_admin, gql_query_as_admin_expect_err,
    install_federation, install_single_shard_federation, knowledge_map_live_query,
    seed_knowledge_map_graph,
};

const INDEX_VERTEX_LABEL: &str = "Person";
const INDEX_AGE_NAME: &str = "pocket_ic_vertex_age";
const INDEX_SCORE_NAME: &str = "pocket_ic_vertex_score";
const INDEX_EDGE_LABEL: &str = "KNOWS";
const INDEX_WEIGHT_NAME: &str = "pocket_ic_edge_weight";
const INDEX_WEIGHT_RIGHT_NAME: &str = "pocket_ic_edge_weight_right";
const INDEX_WEIGHT_UNDIR_NAME: &str = "pocket_ic_edge_weight_undir";
const EDGE_WEIGHT_QUERY: &str = "MATCH ()-[e:KNOWS {weight: 5}]->(b) RETURN e, b";
const EDGE_WEIGHT_UNDIR_QUERY: &str = "MATCH ()~[e:KNOWS {weight: 5}]~() RETURN e";
const EDGE_WEIGHT_UNDIR_BOUND_QUERY: &str = "MATCH ()~[e:KNOWS {weight: 5}]~(b) RETURN e, b";

#[test]
fn router_gql_query_node_scan_on_single_shard() {
    let env = install_single_shard_federation();
    let _ = e2e_insert_vertex(&env, env.graph_source);

    let result = gql_query_as_admin(&env, "MATCH (n) RETURN n");

    assert_eq!(result.row_count, 1);
}

#[test]
fn standalone_e2e_insert_assigns_global_id() {
    let env = install_single_shard_federation();
    let inserted = e2e_insert_vertex(&env, env.graph_source);

    assert_eq!(inserted.global_vertex_id.shard_id, SOURCE_SHARD);
    assert_eq!(
        inserted.global_vertex_id.local_vertex_id,
        inserted.local_vertex_id
    );

    let same_id = GlobalVertexId::new(SOURCE_SHARD, inserted.local_vertex_id);
    assert_eq!(inserted.global_vertex_id, same_id);
}

#[test]
fn standalone_gql_query_index_seeded_property_eq() {
    let env = install_single_shard_federation();
    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "standalone_gql_query_index_seeded_property_eq",
    );
    let _ = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);

    // Inline property equality yields a literal IndexScan anchor (match-level WHERE uses $age).
    let result = gql_query_as_admin(&env, "MATCH (n {age: 5}) RETURN n");

    assert_eq!(result.row_count, 1);
}

#[test]
fn standalone_gql_query_returns_element_id_bytes() {
    let env = install_single_shard_federation();
    let inserted = e2e_insert_vertex(&env, env.graph_source);
    let encoding_key = gleaph_pocket_ic_tests::graph_element_id_encoding_key(
        &env.pic,
        env.admin,
        env.router,
        gleaph_pocket_ic_tests::GRAPH_NAME,
    );

    let result = gql_query_as_admin(&env, "MATCH (n) RETURN ELEMENT_ID(n) AS id");

    assert_eq!(result.row_count, 1);
    let id_bytes = gleaph_pocket_ic_tests::element_id_bytes_from_gql_result(&result, "id");
    assert_eq!(
        GraphPathVertexId::try_from_slice(id_bytes.as_ref())
            .expect("decode vertex id")
            .decode_global(&encoding_key),
        inserted.global_vertex_id
    );
}

#[test]
fn standalone_gql_query_returns_relationship_rows_for_knowledge_map_adapter() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let edge_label = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        edge_label.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(
        &env,
        "MATCH (a)-[e:KNOWS {weight: 5}]->(b) \
         RETURN ELEMENT_ID(a) AS source_id, ELEMENT_ID(e) AS edge_id, \
                ELEMENT_ID(b) AS target_id, e.weight AS edge_weight",
    );

    assert_eq!(result.row_count, 1);
    let encoding_key = gleaph_pocket_ic_tests::graph_element_id_encoding_key(
        &env.pic,
        env.admin,
        env.router,
        gleaph_pocket_ic_tests::GRAPH_NAME,
    );
    let row = decode_single_value_row(&result);
    assert_eq!(
        vertex_id_column(&row, "source_id", &encoding_key),
        source.global_vertex_id,
        "source ELEMENT_ID should identify the seeded source vertex"
    );
    assert_eq!(
        vertex_id_column(&row, "target_id", &encoding_key),
        target.global_vertex_id,
        "target ELEMENT_ID should identify the seeded target vertex"
    );
    let Value::Bytes(edge_id) = row.get("edge_id").expect("edge_id column") else {
        panic!("expected edge_id bytes, got {:?}", row.get("edge_id"));
    };
    GraphPathEdgeId::try_from_slice(edge_id.as_ref()).expect("edge ELEMENT_ID bytes");
    assert_eq!(
        row.get("edge_weight"),
        Some(&Value::Int64(5)),
        "edge property should be projected for adapter row metadata"
    );
}

#[test]
fn router_gql_insert_seeds_knowledge_map_fan_out_graph() {
    let env = install_single_shard_federation();
    seed_knowledge_map_graph(&env);

    let result = gql_query_as_admin(&env, knowledge_map_live_query());
    assert_eq!(
        result.row_count, 26,
        "knowledge-map live query should return one row per seeded demo edge"
    );

    let rows_blob = result
        .rows_blob
        .as_ref()
        .expect("router gql_query should return rows_blob");
    let wire = IcWirePlanQueryResult::decode_blob(rows_blob).expect("decode rows_blob");
    assert_eq!(wire.rows.len(), 26);

    let mut edge_ids = std::collections::BTreeSet::new();
    for row in wire.rows {
        let value_row = row.try_into_value_row().expect("wire row to value row");
        let Value::Text(edge_id) = value_row.get("edge_id").expect("edge_id column") else {
            panic!("expected edge_id text, got {:?}", value_row.get("edge_id"));
        };
        edge_ids.insert(edge_id.clone());
    }

    assert!(
        edge_ids.contains("alice-storage"),
        "expected alice-storage edge, got {edge_ids:?}"
    );
    assert!(
        edge_ids.contains("project-lara"),
        "expected project-lara edge, got {edge_ids:?}"
    );
}

#[test]
fn router_gql_insert_seeds_relationship_rows_for_knowledge_map_adapter() {
    let env = install_single_shard_federation();

    let row_count = gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:Person)-[:KNOWS {weight: 5}]->(:Project)",
        "router_gql_insert_seeds_relationship_rows_for_knowledge_map_adapter",
    );
    assert_eq!(row_count, 0);

    let result = gql_query_as_admin(
        &env,
        "MATCH (a)-[e:KNOWS {weight: 5}]->(b) \
         RETURN ELEMENT_ID(a) AS source_id, ELEMENT_ID(e) AS edge_id, \
                ELEMENT_ID(b) AS target_id, e.weight AS edge_weight",
    );

    assert_eq!(result.row_count, 1);
    let row = decode_single_value_row(&result);
    let Value::Bytes(source_id) = row.get("source_id").expect("source_id column") else {
        panic!("expected source_id bytes, got {:?}", row.get("source_id"));
    };
    GraphPathVertexId::try_from_slice(source_id.as_ref()).expect("decode source ELEMENT_ID bytes");
    let Value::Bytes(target_id) = row.get("target_id").expect("target_id column") else {
        panic!("expected target_id bytes, got {:?}", row.get("target_id"));
    };
    GraphPathVertexId::try_from_slice(target_id.as_ref()).expect("decode target ELEMENT_ID bytes");
    let Value::Bytes(edge_id) = row.get("edge_id").expect("edge_id column") else {
        panic!("expected edge_id bytes, got {:?}", row.get("edge_id"));
    };
    GraphPathEdgeId::try_from_slice(edge_id.as_ref()).expect("decode edge ELEMENT_ID bytes");
    assert_eq!(row.get("edge_weight"), Some(&Value::Int64(5)));
}

#[test]
fn federated_gql_query_index_seeded_routes_to_hit_shard_only() {
    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "federated_gql_query_index_seeded_routes_to_hit_shard_only",
    );
    let _ = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);
    let _ = e2e_insert_vertex_with_property(&env, env.graph_dest, age.raw(), 9);

    let result = gql_query_as_admin(&env, "MATCH (n {age: 5}) RETURN n");

    assert_eq!(result.row_count, 1);
}

/// Two indexed equality predicates on one variable produce a `PlanOp::IndexIntersection`, now
/// served by the streaming `lookup_equal_page` + `filter_hits_by_equal` path on graph-index.
#[test]
fn standalone_gql_query_index_intersection_two_properties() {
    let env = install_single_shard_federation();
    let age = admin_intern_property(&env, "age");
    let score = admin_intern_property(&env, "score");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "standalone_intersection_age",
    );
    create_vertex_property_index(
        &env,
        INDEX_SCORE_NAME,
        INDEX_VERTEX_LABEL,
        "score",
        "standalone_intersection_score",
    );
    // Matches both arms.
    let _ =
        e2e_insert_vertex_with_two_properties(&env, env.graph_source, age.raw(), 5, score.raw(), 9);
    // Matches only the age arm — must be sieved out by the score `contains` check.
    let _ = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);

    let result = gql_query_as_admin(&env, "MATCH (n {age: 5, score: 9}) RETURN n");

    assert_eq!(
        result.row_count, 1,
        "intersection should return only the vertex matching both arms"
    );
}

#[test]
fn federated_gql_query_index_intersection_merges_matching_shards() {
    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    let score = admin_intern_property(&env, "score");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "federated_intersection_age",
    );
    create_vertex_property_index(
        &env,
        INDEX_SCORE_NAME,
        INDEX_VERTEX_LABEL,
        "score",
        "federated_intersection_score",
    );
    // Full match on each shard.
    let _ =
        e2e_insert_vertex_with_two_properties(&env, env.graph_source, age.raw(), 5, score.raw(), 9);
    let _ =
        e2e_insert_vertex_with_two_properties(&env, env.graph_dest, age.raw(), 5, score.raw(), 9);
    // Partial match (age only) on dest — must be excluded.
    let _ = e2e_insert_vertex_with_property(&env, env.graph_dest, age.raw(), 5);

    let result = gql_query_as_admin(&env, "MATCH (n {age: 5, score: 9}) RETURN n");

    assert_eq!(
        result.row_count, 2,
        "streamed intersection should merge full matches across both shards"
    );
}

fn decode_single_value_row(
    result: &gleaph_graph_kernel::plan_exec::GqlQueryResult,
) -> std::collections::BTreeMap<String, Value> {
    let rows_blob = result
        .rows_blob
        .as_ref()
        .expect("router gql_query should return rows_blob");
    let wire = IcWirePlanQueryResult::decode_blob(rows_blob).expect("decode rows_blob");
    assert_eq!(wire.rows.len(), 1);
    wire.rows
        .into_iter()
        .next()
        .expect("one row")
        .try_into_value_row()
        .expect("wire row to value row")
}

fn vertex_id_column(
    row: &std::collections::BTreeMap<String, Value>,
    column: &str,
    encoding_key: &ElementIdEncodingKey,
) -> GlobalVertexId {
    let Value::Bytes(id_bytes) = row.get(column).unwrap_or_else(|| panic!("{column} column"))
    else {
        panic!("expected {column} bytes, got {:?}", row.get(column));
    };
    GraphPathVertexId::try_from_slice(id_bytes.as_ref())
        .expect("decode vertex id")
        .decode_global(encoding_key)
}

#[test]
fn federated_gql_query_index_seeded_merges_across_shards() {
    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "federated_gql_query_index_seeded_merges_across_shards",
    );
    let source = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);
    let dest = e2e_insert_vertex_with_property(&env, env.graph_dest, age.raw(), 5);

    assert_eq!(source.global_vertex_id.shard_id, SOURCE_SHARD);
    assert_eq!(dest.global_vertex_id.shard_id, DEST_SHARD);

    let result = gql_query_as_admin(&env, "MATCH (n {age: 5}) RETURN n");

    assert_eq!(result.row_count, 2);
}

#[test]
fn standalone_drop_index_property_eq_still_queries_via_scan() {
    let env = install_single_shard_federation();
    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "standalone_drop_index_create",
    );
    let _ = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);

    let indexed = gql_query_as_admin(&env, "MATCH (n {age: 5}) RETURN n");
    assert_eq!(indexed.row_count, 1);

    drop_vertex_property_index(&env, INDEX_AGE_NAME, true, "standalone_drop_index_drop");

    let all_nodes = gql_query_as_admin(&env, "MATCH (n) RETURN n");
    assert_eq!(
        all_nodes.row_count, 1,
        "vertex should still exist after DROP INDEX"
    );

    let after_drop = gql_query_as_admin(&env, "MATCH (n {age: 5}) RETURN n");
    assert_eq!(
        after_drop.row_count, 1,
        "single-shard scan path should still match after DROP INDEX"
    );
}

#[test]
fn federated_drop_index_property_eq_loses_federated_anchor() {
    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "federated_drop_index_create",
    );
    let _ = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);
    let _ = e2e_insert_vertex_with_property(&env, env.graph_dest, age.raw(), 5);

    let indexed = gql_query_as_admin(&env, "MATCH (n {age: 5}) RETURN n");
    assert_eq!(indexed.row_count, 2);

    drop_vertex_property_index(&env, INDEX_AGE_NAME, true, "federated_drop_index_drop");

    let err = gql_query_as_admin_expect_err(&env, "MATCH (n {age: 5}) RETURN n");
    assert!(
        err.to_string().contains("no index anchor"),
        "expected federated dispatch without index anchor to fail, got: {err:?}"
    );
}

#[test]
fn drop_index_if_exists_is_idempotent() {
    let env = install_single_shard_federation();
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "drop_index_if_exists_create",
    );
    drop_vertex_property_index(&env, INDEX_AGE_NAME, true, "drop_index_if_exists_first");
    drop_vertex_property_index(&env, INDEX_AGE_NAME, true, "drop_index_if_exists_second");
}

#[test]
fn standalone_gql_query_edge_index_seeded_property_eq() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_edge_property_index(
        &env,
        INDEX_WEIGHT_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "standalone_gql_query_edge_index_seeded_property_eq",
    );
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, EDGE_WEIGHT_QUERY);

    assert_eq!(result.row_count, 1);
}

#[test]
fn standalone_gql_query_edge_index_pointing_right_ddl() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_directed_edge_property_index(
        &env,
        INDEX_WEIGHT_RIGHT_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "standalone_gql_query_edge_index_pointing_right_ddl",
    );
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, EDGE_WEIGHT_QUERY);

    assert_eq!(result.row_count, 1);
}

#[test]
fn standalone_gql_query_edge_index_undirected_ddl() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_undirected_edge_property_index(
        &env,
        INDEX_WEIGHT_UNDIR_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "standalone_gql_query_edge_index_undirected_ddl",
    );
    let v = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_undirected_edge_with_property(
        &env,
        env.graph_source,
        v.local_vertex_id,
        v.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, EDGE_WEIGHT_UNDIR_QUERY);

    assert_eq!(result.row_count, 1);
}

/// Undirected-only index maintains undirected wire postings; directed inserts must not seed
/// an undirected leading `EdgeIndexScan` (ADR 0012 subset rule).
#[test]
fn standalone_gql_query_undirected_index_does_not_seed_directed_edge() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_undirected_edge_property_index(
        &env,
        INDEX_WEIGHT_UNDIR_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "standalone_gql_query_undirected_index_does_not_seed_directed_edge",
    );
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, EDGE_WEIGHT_UNDIR_BOUND_QUERY);

    assert_eq!(result.row_count, 0);
}

/// Anonymous endpoints on both sides of an undirected edge match once per endpoint
/// when the planner expands from each vertex (no leading edge index anchor).
#[test]
fn standalone_gql_query_undirected_symmetric_anonymous_endpoints() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_undirected_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, "MATCH ()~[e:KNOWS]~() WHERE e.weight = 5 RETURN e");

    assert_eq!(result.row_count, 2);
}

#[test]
fn federated_gql_query_edge_index_undirected_ddl() {
    let env = install_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_undirected_edge_property_index(
        &env,
        INDEX_WEIGHT_UNDIR_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "federated_gql_query_edge_index_undirected_ddl",
    );
    let source_a = e2e_insert_vertex(&env, env.graph_source);
    let target_a = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_undirected_edge_with_property(
        &env,
        env.graph_source,
        source_a.local_vertex_id,
        target_a.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );
    let source_b = e2e_insert_vertex(&env, env.graph_dest);
    let target_b = e2e_insert_vertex(&env, env.graph_dest);
    e2e_insert_undirected_edge_with_property(
        &env,
        env.graph_dest,
        source_b.local_vertex_id,
        target_b.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, EDGE_WEIGHT_UNDIR_BOUND_QUERY);

    assert_eq!(result.row_count, 2);
}

#[test]
fn federated_gql_query_edge_index_pointing_right_ddl() {
    let env = install_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_directed_edge_property_index(
        &env,
        INDEX_WEIGHT_RIGHT_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "federated_gql_query_edge_index_pointing_right_ddl",
    );
    let source_a = e2e_insert_vertex(&env, env.graph_source);
    let target_a = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source_a.local_vertex_id,
        target_a.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );
    let source_b = e2e_insert_vertex(&env, env.graph_dest);
    let target_b = e2e_insert_vertex(&env, env.graph_dest);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_dest,
        source_b.local_vertex_id,
        target_b.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, EDGE_WEIGHT_QUERY);

    assert_eq!(result.row_count, 2);
}

#[test]
fn standalone_drop_edge_index_property_eq_still_queries_via_scan() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_edge_property_index(
        &env,
        INDEX_WEIGHT_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "standalone_drop_edge_index_create",
    );
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let indexed = gql_query_as_admin(&env, EDGE_WEIGHT_QUERY);
    assert_eq!(indexed.row_count, 1);

    drop_vertex_property_index(
        &env,
        INDEX_WEIGHT_NAME,
        true,
        "standalone_drop_edge_index_drop",
    );

    let all_edges = gql_query_as_admin(&env, "MATCH ()-[e:KNOWS]->(b) RETURN e, b");
    assert_eq!(
        all_edges.row_count, 1,
        "edge should still exist after DROP INDEX"
    );

    let after_drop = gql_query_as_admin(&env, EDGE_WEIGHT_QUERY);
    assert_eq!(
        after_drop.row_count, 1,
        "single-shard scan path should still match after DROP INDEX"
    );
}

#[test]
fn federated_drop_edge_index_property_eq_loses_federated_anchor() {
    let env = install_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_edge_property_index(
        &env,
        INDEX_WEIGHT_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "federated_drop_edge_index_create",
    );
    let source_a = e2e_insert_vertex(&env, env.graph_source);
    let target_a = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source_a.local_vertex_id,
        target_a.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );
    let source_b = e2e_insert_vertex(&env, env.graph_dest);
    let target_b = e2e_insert_vertex(&env, env.graph_dest);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_dest,
        source_b.local_vertex_id,
        target_b.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let indexed = gql_query_as_admin(&env, EDGE_WEIGHT_QUERY);
    assert_eq!(indexed.row_count, 2);

    drop_vertex_property_index(
        &env,
        INDEX_WEIGHT_NAME,
        true,
        "federated_drop_edge_index_drop",
    );

    let err = gql_query_as_admin_expect_err(&env, EDGE_WEIGHT_QUERY);
    assert!(
        err.to_string().contains("no index anchor"),
        "expected federated dispatch without index anchor to fail, got: {err:?}"
    );
}

#[test]
fn drop_index_without_if_exists_errors_when_missing() {
    let env = install_single_shard_federation();
    let err = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &format!("DROP INDEX {INDEX_AGE_NAME}"),
        "drop_index_missing",
    );
    assert!(
        matches!(
            err,
            gleaph_graph_kernel::federation::RouterError::NotFound(_)
        ),
        "expected NotFound for missing index, got: {err:?}"
    );
}
