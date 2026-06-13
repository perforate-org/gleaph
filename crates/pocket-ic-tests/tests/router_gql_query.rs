//! PocketIC: router `gql_query` composite path (parse → plan → graph dispatch).
//!
//! Covers single-shard NodeScan, index-seeded property equality, `ELEMENT_ID` rows, and
//! multi-shard router index lookup with per-shard `seed_bindings_blob` fan-out.

use gleaph_gql::Value;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, GlobalVertexId, VertexPlacement};
use gleaph_graph_kernel::path::GraphPathVertexId;
use gleaph_pocket_ic_tests::{
    DEST_SHARD, SOURCE_SHARD, admin_intern_property, create_vertex_property_index,
    drop_vertex_property_index, e2e_insert_vertex, e2e_insert_vertex_with_property,
    gql_execute_idempotent_as_admin_expect_err, gql_query_as_admin, gql_query_as_admin_expect_err,
    install_federation, install_single_shard_federation, resolve_placement,
};

const INDEX_VERTEX_LABEL: &str = "Person";
const INDEX_AGE_NAME: &str = "pocket_ic_vertex_age";

#[test]
fn router_gql_query_node_scan_on_single_shard() {
    let env = install_single_shard_federation();
    let _ = e2e_insert_vertex(&env, env.graph_source);

    let result = gql_query_as_admin(&env, "MATCH (n) RETURN n");

    assert_eq!(result.row_count, 1);
}

#[test]
fn standalone_e2e_insert_commits_placement_and_global_id() {
    let env = install_single_shard_federation();
    let inserted = e2e_insert_vertex(&env, env.graph_source);

    assert_eq!(inserted.global_vertex_id.shard_id, SOURCE_SHARD);
    assert_eq!(
        inserted.global_vertex_id.local_vertex_id,
        inserted.local_vertex_id
    );

    let placement = resolve_placement(&env, inserted.global_vertex_id);
    assert!(matches!(
        placement,
        VertexPlacement::Active(loc)
            if loc.shard_id == SOURCE_SHARD && loc.local_vertex_id == inserted.local_vertex_id
    ));

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

    let result = gql_query_as_admin(&env, "MATCH (n) RETURN ELEMENT_ID(n) AS id");

    assert_eq!(result.row_count, 1);
    let rows_blob = result
        .rows_blob
        .as_ref()
        .expect("router gql_query should return rows_blob for ELEMENT_ID projection");
    let wire = IcWirePlanQueryResult::decode_blob(rows_blob).expect("decode rows_blob");
    assert_eq!(wire.rows.len(), 1);
    let row = wire
        .rows
        .into_iter()
        .next()
        .expect("one row")
        .try_into_value_row()
        .expect("wire row to value row");
    let Value::Bytes(id_bytes) = row.get("id").expect("id column") else {
        panic!("expected ELEMENT_ID bytes, got {:?}", row.get("id"));
    };
    assert_eq!(
        GraphPathVertexId::try_from_slice(id_bytes.as_ref())
            .expect("decode vertex id")
            .decode_global(&ElementIdEncodingKey::standalone()),
        inserted.global_vertex_id
    );
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
