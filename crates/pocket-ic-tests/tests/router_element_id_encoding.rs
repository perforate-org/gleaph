//! PocketIC: ADR 0019 §3.1 per-graph element-id encoding keys.

use gleaph_graph_kernel::federation::{ElementIdEncodingKey, ShardId};
use gleaph_graph_kernel::path::GraphPathVertexId;
use gleaph_pocket_ic_tests::{
    GRAPH_HOME_NAME, GRAPH_REMOTE_NAME, e2e_insert_vertex_via_router,
    element_id_bytes_from_gql_result, gql_query_on_router, graph_element_id_encoding_key,
    install_two_graph_two_index_federation,
};

#[test]
fn two_graphs_same_canonical_vertex_get_distinct_encoded_element_ids() {
    let env = install_two_graph_two_index_federation();

    let home_insert = e2e_insert_vertex_via_router(&env.pic, env.router, env.graph_home);
    let remote_insert = e2e_insert_vertex_via_router(&env.pic, env.router, env.graph_remote);

    assert_eq!(home_insert.global_vertex_id.shard_id, ShardId::new(0));
    assert_eq!(remote_insert.global_vertex_id.shard_id, ShardId::new(0));
    assert_eq!(
        home_insert.global_vertex_id.local_vertex_id,
        remote_insert.global_vertex_id.local_vertex_id,
        "both graphs start from the same local vertex id on their sole shard"
    );

    let home_key = graph_element_id_encoding_key(&env.pic, env.admin, env.router, GRAPH_HOME_NAME);
    let remote_key =
        graph_element_id_encoding_key(&env.pic, env.admin, env.router, GRAPH_REMOTE_NAME);
    assert_ne!(home_key, remote_key);
    assert_ne!(home_key, ElementIdEncodingKey::host_test_fixture());
    assert_ne!(remote_key, ElementIdEncodingKey::host_test_fixture());

    let home_query = format!("USE {GRAPH_HOME_NAME} MATCH (n) RETURN ELEMENT_ID(n) AS id");
    let remote_query = format!("USE {GRAPH_REMOTE_NAME} MATCH (n) RETURN ELEMENT_ID(n) AS id");

    let home_result = gql_query_on_router(&env.pic, env.admin, env.router, &home_query);
    let remote_result = gql_query_on_router(&env.pic, env.admin, env.router, &remote_query);

    let home_bytes = element_id_bytes_from_gql_result(&home_result, "id");
    let remote_bytes = element_id_bytes_from_gql_result(&remote_result, "id");
    assert_ne!(
        home_bytes, remote_bytes,
        "identical GlobalVertexId under different graphs must not share encoded bytes"
    );

    let home_path = GraphPathVertexId::try_from_slice(home_bytes.as_ref()).expect("home id");
    let remote_path = GraphPathVertexId::try_from_slice(remote_bytes.as_ref()).expect("remote id");

    assert_eq!(
        home_path.decode_global(&home_key),
        home_insert.global_vertex_id
    );
    assert_eq!(
        remote_path.decode_global(&remote_key),
        remote_insert.global_vertex_id
    );

    assert_ne!(
        home_path.decode_global(&remote_key),
        home_insert.global_vertex_id,
        "encoded id must decode under the issuing graph's key only"
    );
    assert_ne!(
        remote_path.decode_global(&home_key),
        remote_insert.global_vertex_id,
        "encoded id must decode under the issuing graph's key only"
    );
}
