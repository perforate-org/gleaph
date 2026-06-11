//! PocketIC: router `gql_query` composite path (parse → plan → graph dispatch).
//!
//! Multi-shard graphs require a router-owned index anchor; this file covers single-shard
//! NodeScan and standalone placement after `e2e_insert_vertex`. Index-seeded multi-shard
//! `gql_query` remains planned ([federation-target.md](../../../design/sharding/federation-target.md)).

use gleaph_gql::Value;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, GlobalVertexId, VertexPlacement};
use gleaph_graph_kernel::path::GraphPathVertexId;
use gleaph_pocket_ic_tests::{
    SOURCE_SHARD, admin_intern_property, admin_set_indexed_vertex_property, e2e_insert_vertex,
    e2e_insert_vertex_with_property, gql_query_as_admin, install_single_shard_federation,
    resolve_placement,
};

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
    admin_set_indexed_vertex_property(&env, "age");
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
    let row = wire.rows.into_iter().next().expect("one row").try_into_value_row().expect("wire row to value row");
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
