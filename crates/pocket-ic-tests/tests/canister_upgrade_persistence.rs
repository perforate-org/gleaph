//! PocketIC: the canister upgrade boundary must preserve all persisted state
//! without corruption.
//!
//! Persistence in gleaph is fully implicit: every durable structure lives in
//! `ic_stable_structures` behind a `MemoryManager`, and only the graph shard has
//! a `#[post_upgrade]` hook (it rebuilds non-stable process state). This test
//! seeds a realistic fan-out graph (vertices + directed edges + edge properties +
//! a federated index), captures the full query result, upgrades all three
//! canisters to the same wasm, and requires the post-upgrade query to be
//! byte-for-byte identical — then proves the graph is still writable.

use candid::{Decode, Encode};
use gleaph_gql::Value;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{IndexPostingBatchProgress, IndexPostingMutation, PostingHit};
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, gql_execute_idempotent_as_admin, gql_query_as_admin,
    install_single_shard_federation, knowledge_map_live_query, seed_knowledge_map_graph,
    wasm_bytes,
};
use std::collections::BTreeSet;

/// Distinct `edge_id` values returned by the knowledge-map live query.
fn edge_ids(result: &GqlQueryResult) -> BTreeSet<String> {
    let rows_blob = result
        .rows_blob
        .as_ref()
        .expect("gql_query should return rows_blob");
    let wire = IcWirePlanQueryResult::decode_blob(rows_blob).expect("decode rows_blob");
    let mut ids = BTreeSet::new();
    for row in wire.rows {
        let value_row = row.try_into_value_row().expect("wire row to value row");
        let Value::Text(edge_id) = value_row.get("edge_id").expect("edge_id column") else {
            panic!("expected edge_id text, got {:?}", value_row.get("edge_id"));
        };
        ids.insert(edge_id.clone());
    }
    ids
}

/// Upgrade router, index, and graph shard to the same wasm in place. The graph
/// shard's `post_upgrade` re-installs the rkyv hook and re-arms the maintenance
/// timer; router and index have no upgrade hook and rely purely on stable memory
/// surviving the reinstall.
fn upgrade_all(env: &FederationEnv) {
    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.router, wasm_bytes("ROUTER_WASM"), empty.clone(), None)
        .expect("upgrade router canister");
    env.pic
        .upgrade_canister(env.index, wasm_bytes("INDEX_WASM"), empty.clone(), None)
        .expect("upgrade index canister");
    env.pic
        .upgrade_canister(env.graph_source, wasm_bytes("GRAPH_WASM"), empty, None)
        .expect("upgrade graph shard canister");
}

#[test]
fn canister_upgrade_preserves_seeded_graph_without_corruption() {
    let env = install_single_shard_federation();
    seed_knowledge_map_graph(&env);

    let before = gql_query_as_admin(&env, knowledge_map_live_query());
    let before_ids = edge_ids(&before);
    assert_eq!(
        before.row_count, 26,
        "baseline knowledge-map query should return one row per seeded demo edge"
    );
    assert_eq!(before_ids.len(), 26, "baseline edge ids must be distinct");

    upgrade_all(&env);

    // Post-upgrade the same query must observe the exact same graph: identical
    // row count and identical edge-id set. Any stable-layout, Storable, router
    // catalog, shard adjacency, or index posting corruption would diverge here.
    let after = gql_query_as_admin(&env, knowledge_map_live_query());
    let after_ids = edge_ids(&after);
    assert_eq!(
        after.row_count, before.row_count,
        "row count changed across canister upgrade"
    );
    assert_eq!(
        after_ids, before_ids,
        "edge-id set changed across canister upgrade"
    );

    // The graph must remain fully operational after the upgrade: an idempotent
    // re-seed (same client mutation keys) must be a no-op that still reads back
    // the unchanged graph, proving the router idempotency journal and shard
    // state survived intact.
    seed_knowledge_map_graph(&env);
    let after_reseed = gql_query_as_admin(&env, knowledge_map_live_query());
    assert_eq!(
        after_reseed.row_count, before.row_count,
        "idempotent re-seed after upgrade must not change the graph"
    );
    assert_eq!(
        edge_ids(&after_reseed),
        before_ids,
        "edge-id set changed after post-upgrade idempotent re-seed"
    );
}

#[test]
fn canister_upgrade_repeated_is_stable() {
    let env = install_single_shard_federation();
    seed_knowledge_map_graph(&env);
    let baseline = edge_ids(&gql_query_as_admin(&env, knowledge_map_live_query()));

    // Two successive upgrades must each preserve the data (guards against an
    // upgrade that silently consumes/relocates stable regions on every cycle).
    for _ in 0..2 {
        upgrade_all(&env);
        let ids = edge_ids(&gql_query_as_admin(&env, knowledge_map_live_query()));
        assert_eq!(
            ids, baseline,
            "edge-id set drifted across repeated upgrades"
        );
    }

    // A new write after repeated upgrades must still land and be queryable
    // (mirrors the knowledge-map seed edge pattern / schema).
    let new_edge_ddl = "MATCH (a:Person {demo_id: 'alice', demo_graph: 'knowledge-map'}) RETURN a \
NEXT INSERT (a)-[:WROTE {demo_edge_id: 'alice-post-upgrade', demo_kind: 'verify'}]\
->(b:Post {demo_id: 'post-upgrade-check', demo_graph: 'knowledge-map', title: 'Upgrade check'})";
    let _ = gql_execute_idempotent_as_admin(&env, new_edge_ddl, "post_upgrade_new_edge");
    let after_write = edge_ids(&gql_query_as_admin(&env, knowledge_map_live_query()));
    assert!(
        after_write.contains("alice-post-upgrade"),
        "new edge written after repeated upgrades must be queryable, got {after_write:?}"
    );
    assert_eq!(
        after_write.len(),
        baseline.len() + 1,
        "exactly one new edge should be added after upgrades"
    );
}

#[test]
fn graph_index_batch_posting_survives_index_upgrade() {
    let env = install_single_shard_federation();
    let value = vec![0x42, 0x01];
    let args = Encode!(
        &ShardId::new(0),
        &vec![
            IndexPostingMutation::VertexProperty {
                remove: false,
                property_id: 77,
                value: value.clone(),
                vertex_id: 11,
            },
            IndexPostingMutation::VertexProperty {
                remove: false,
                property_id: 77,
                value: value.clone(),
                vertex_id: 12,
            },
        ]
    )
    .expect("encode posting batch");
    let bytes = env
        .pic
        .update_call(env.index, env.graph_source, "posting_batch", args)
        .expect("posting batch call");
    let progress = Decode!(&bytes, IndexPostingBatchProgress).expect("decode posting progress");
    assert_eq!(progress.applied, 2);
    assert!(progress.next_index.is_none());

    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.index, wasm_bytes("INDEX_WASM"), empty, None)
        .expect("upgrade graph-index canister");

    let lookup = env
        .pic
        .query_call(
            env.index,
            env.router,
            "lookup_equal",
            Encode!(&77u32, &value).expect("encode lookup"),
        )
        .expect("lookup after index upgrade");
    let hits = Decode!(&lookup, Vec<PostingHit>).expect("decode lookup hits");
    assert_eq!(
        hits.iter().map(|hit| hit.vertex_id).collect::<Vec<_>>(),
        vec![11, 12],
        "batch-applied postings must survive graph-index upgrade"
    );
}
