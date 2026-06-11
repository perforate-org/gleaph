//! PocketIC: router `gql_query` composite path (parse → plan → graph dispatch).
//!
//! Multi-shard graphs require a router-owned index anchor; this file covers single-shard
//! NodeScan and standalone placement after `e2e_insert_vertex`. Index-seeded multi-shard
//! `gql_query` remains planned ([federation-target.md](../../../design/sharding/federation-target.md)).

use gleaph_graph_kernel::federation::{GlobalVertexId, VertexPlacement};
use gleaph_pocket_ic_tests::{
    SOURCE_SHARD, e2e_insert_vertex, gql_query_as_admin, install_single_shard_federation,
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
