//! PocketIC: router `gql_query` composite path (parse → plan → graph dispatch).
//!
//! Multi-shard graphs require a router-owned index anchor; this file starts with a single-shard
//! NodeScan smoke test. Index-seeded multi-shard `gql_query` is tracked in the Phase 7 roadmap.

use gleaph_pocket_ic_tests::{
    e2e_insert_vertex, gql_query_as_admin, install_single_shard_federation,
};

#[test]
fn router_gql_query_node_scan_on_single_shard() {
    let env = install_single_shard_federation();
    let _ = e2e_insert_vertex(&env, env.graph_source);

    let result = gql_query_as_admin(&env, "MATCH (n) RETURN n");

    assert_eq!(result.row_count, 1);
}
