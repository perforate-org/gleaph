//! PocketIC red repros for ADR 0023 (federated index/store consistency across
//! the upgrade boundary).
//!
//! The shard-local index registry (`graph/src/index/registry.rs`) is a volatile
//! `thread_local` derived gate, and `#[post_upgrade]` does not rebuild it. As a
//! result a write that lands after a graph-shard upgrade emits no index posting
//! (the registry reports the property as not-indexed), while the router's index
//! catalog — which is stable — keeps planning index-served lookups. The index
//! and the primary store silently diverge: index-served queries miss the
//! post-upgrade write.
//!
//! This test goes through the real GQL mutation path (router → planner →
//! `ExecutePlanArgs` → shard), which is exactly the path ADR 0023 rewires to
//! carry a router-sourced catalog. It is RED today and becomes the GREEN target
//! once the registry is replaced by the ephemeral per-operation catalog.

use candid::Encode;
use gleaph_pocket_ic_tests::{
    create_vertex_property_index, gql_execute_idempotent_as_admin, gql_query_as_admin,
    install_single_shard_federation, wasm_bytes,
};

const INDEX_VERTEX_LABEL: &str = "Person";
const INDEX_AGE_NAME: &str = "adr0023_vertex_age";

#[test]
fn post_upgrade_indexed_write_stays_consistent_with_store() {
    let env = install_single_shard_federation();
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "adr0023_create_index",
    );

    // Pre-upgrade indexed write: the posting is created and the index-served
    // equality lookup finds exactly the one matching vertex.
    let _ = gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:Person {age: 5})",
        "adr0023_pre_upgrade_insert",
    );
    let before = gql_query_as_admin(&env, "MATCH (n {age: 5}) RETURN n");
    assert_eq!(
        before.row_count, 1,
        "pre-upgrade indexed query must find the single matching vertex"
    );

    // Upgrade only the graph shard: its volatile registry is dropped and
    // post_upgrade does not rebuild it (ADR 0023 P1). The router keeps the index
    // in its (stable) catalog, so it still plans index-served lookups.
    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.graph_source, wasm_bytes("GRAPH_WASM"), empty, None)
        .expect("upgrade graph shard canister");

    // Post-upgrade indexed write of a second vertex with the SAME indexed value.
    let _ = gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:Person {age: 5})",
        "adr0023_post_upgrade_insert",
    );

    // The index-served query must now observe BOTH vertices. Today it returns
    // only 1: the post-upgrade write emitted no posting (registry empty), so the
    // index diverged from the store. This is the red repro for P1.
    let after = gql_query_as_admin(&env, "MATCH (n {age: 5}) RETURN n");
    assert_eq!(
        after.row_count, 2,
        "post-upgrade indexed write must be visible through the index \
         (P1: shard registry volatility loses the posting)"
    );
}
