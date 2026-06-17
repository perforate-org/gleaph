//! PocketIC: ADR 0019 S5 multi-graph index-cluster isolation.

use candid::{Decode, Encode};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_pocket_ic_tests::{
    GRAPH_HOME_NAME, GRAPH_REMOTE_NAME, install_two_graph_two_index_federation,
};
use gleaph_router::state::RouterError;
use gleaph_router::types::ShardRegistryEntry;

fn lookup_graph_id(env: &gleaph_pocket_ic_tests::TwoGraphTwoIndexEnv, graph_name: &str) -> GraphId {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "lookup_graph_id",
            Encode!(&graph_name.to_string()).expect("encode lookup_graph_id"),
        )
        .expect("lookup_graph_id");
    match Decode!(&bytes, Result<GraphId, RouterError>) {
        Ok(Ok(graph_id)) => graph_id,
        Ok(Err(err)) => panic!("lookup_graph_id rejected: {err:?}"),
        Err(err) => panic!("decode lookup_graph_id: {err}"),
    }
}

fn list_shards(
    env: &gleaph_pocket_ic_tests::TwoGraphTwoIndexEnv,
    graph_name: &str,
) -> Vec<ShardRegistryEntry> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "list_shards_for_graph",
            Encode!(&graph_name.to_string()).expect("encode list_shards_for_graph"),
        )
        .expect("list_shards_for_graph");
    match Decode!(&bytes, Result<Vec<ShardRegistryEntry>, RouterError>) {
        Ok(Ok(shards)) => shards,
        Ok(Err(err)) => panic!("list_shards_for_graph rejected: {err:?}"),
        Err(err) => panic!("decode list_shards_for_graph: {err}"),
    }
}

#[test]
fn two_graphs_can_use_shard_zero_with_distinct_index_clusters() {
    let env = install_two_graph_two_index_federation();

    let home_shards = list_shards(&env, GRAPH_HOME_NAME);
    let remote_shards = list_shards(&env, GRAPH_REMOTE_NAME);
    assert_eq!(home_shards.len(), 1);
    assert_eq!(remote_shards.len(), 1);
    assert_eq!(home_shards[0].shard_id, ShardId::new(0));
    assert_eq!(remote_shards[0].shard_id, ShardId::new(0));
    assert_eq!(home_shards[0].index_canister, env.index_home);
    assert_eq!(remote_shards[0].index_canister, env.index_remote);
    assert_ne!(env.index_home, env.index_remote);
}

#[test]
fn index_rejects_attach_for_foreign_graph_owner() {
    let env = install_two_graph_two_index_federation();
    let remote_graph_id = lookup_graph_id(&env, GRAPH_REMOTE_NAME);

    let bytes = env
        .pic
        .update_call(
            env.index_home,
            env.router,
            "admin_attach_shard_canister",
            Encode!(
                &remote_graph_id,
                &1u32,
                &0u32,
                &ShardId::new(0),
                &env.graph_remote
            )
            .expect("encode admin_attach_shard_canister"),
        )
        .expect("admin_attach_shard_canister");

    let result: Result<(), String> =
        Decode!(&bytes, Result<(), String>).expect("decode admin_attach_shard_canister");
    let err = result.expect_err("foreign graph attach should be rejected");
    assert!(
        err.contains("already bound to a different graph/group"),
        "unexpected error: {err}"
    );
}
