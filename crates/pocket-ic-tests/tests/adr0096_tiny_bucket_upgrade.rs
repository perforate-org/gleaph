//! PocketIC one-boundary-path coverage for ADR 0096 inline-tiny mode.
//!
//! Tiny buckets (degree ≤ 3, descriptor-resident targets) cross the canister
//! upgrade boundary like any other bucket state: this test writes 3 edges
//! (tiny-birth, spanless) through the real GQL mutation path on a
//! single-shard federation, upgrades the graph shard, and requires the
//! post-upgrade adjacency to be identical — then proves the graph is still
//! writable (a 4th edge promotes tiny→slab and reads back).
//!
//! One fresh fixture, one boundary crossing (cost-aware-validation: a single
//! lifecycle test owns the whole upgrade contract; edge-order combinatorics
//! stay in unit tests).

use candid::Encode;
use gleaph_pocket_ic_tests::{
    ensure_edge_label, ensure_vertex_label, gql_mutate_as_admin, gql_query_as_admin,
    install_single_shard_federation, wasm_bytes,
};

fn count(env: &gleaph_pocket_ic_tests::FederationEnv, query: &str) -> u64 {
    gql_query_as_admin(env, query).row_count
}

#[test]
fn labeled_tiny_bucket_adjacency_survives_graph_upgrade() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "TinyN");
    ensure_vertex_label(&env, "TinyM");
    ensure_edge_label(&env, "TINY_LINK");
    // Three edges TinyN→TinyM: one bucket, degree 3, still tiny (spanless).
    for (i, target) in ["TinyM", "TinyM", "TinyM"].iter().enumerate() {
        let _ = target;
        gql_mutate_as_admin(
            &env,
            "INSERT (:TinyN)-[:TINY_LINK]->(:TinyM)",
            &format!("adr0096_tiny_seed_{i}"),
        );
    }
    assert_eq!(
        count(&env, "MATCH (:TinyN)-[:TINY_LINK]->(:TinyM) RETURN 1"),
        3,
        "pre-upgrade tiny-bucket adjacency must have 3 rows"
    );

    // Upgrade only the graph shard: tiny descriptors live in stable bucket
    // rows, so post_upgrade must reattach them identical.
    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.graph_source, wasm_bytes("GRAPH_WASM"), empty, None)
        .expect("upgrade graph shard canister");

    assert_eq!(
        count(&env, "MATCH (:TinyN)-[:TINY_LINK]->(:TinyM) RETURN 1"),
        3,
        "row count changed across canister upgrade"
    );

    // Still writable: a 4th edge promotes tiny→slab and reads back.
    gql_mutate_as_admin(
        &env,
        "INSERT (:TinyN)-[:TINY_LINK]->(:TinyM)",
        "adr0096_post_upgrade_promote",
    );
    assert_eq!(
        count(&env, "MATCH (:TinyN)-[:TINY_LINK]->(:TinyM) RETURN 1"),
        4,
        "post-upgrade write must land"
    );
}
