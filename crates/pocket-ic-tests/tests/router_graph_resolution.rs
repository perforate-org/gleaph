//! PocketIC: ADR 0011 graph resolution — HOME default and remote top-level `USE GRAPH`.

use gleaph_pocket_ic_tests::{
    GRAPH_HOME_NAME, GRAPH_REMOTE_NAME, e2e_insert_vertex, gql_query_as_admin,
    install_two_graph_federation,
};

#[test]
fn gql_query_home_graph_default_without_session_set() {
    let env = install_two_graph_federation();
    let _ = e2e_insert_vertex(&env, env.graph_source);

    let result = gql_query_as_admin(&env, "MATCH (n) RETURN n");

    assert_eq!(result.row_count, 1);
}

#[test]
fn gql_query_home_graph_does_not_dispatch_to_remote_shard() {
    let env = install_two_graph_federation();
    let _ = e2e_insert_vertex(&env, env.graph_dest);

    let result = gql_query_as_admin(&env, "MATCH (n) RETURN n");

    assert_eq!(result.row_count, 0);
}

#[test]
fn gql_query_remote_use_graph_defocuses_to_focused_graph() {
    let env = install_two_graph_federation();
    let _ = e2e_insert_vertex(&env, env.graph_dest);

    let query =
        format!("SESSION SET GRAPH {GRAPH_HOME_NAME} USE {GRAPH_REMOTE_NAME} MATCH (n) RETURN n");
    let result = gql_query_as_admin(&env, &query);

    assert_eq!(result.row_count, 1);
}

#[test]
fn gql_query_nested_use_graph_defocuses_to_innermost() {
    let env = install_two_graph_federation();
    let _ = e2e_insert_vertex(&env, env.graph_dest);

    let query =
        format!("USE {GRAPH_HOME_NAME} {{ USE {GRAPH_REMOTE_NAME} {{ MATCH (n) RETURN n }} }}");
    let result = gql_query_as_admin(&env, &query);

    assert_eq!(result.row_count, 1);
}
