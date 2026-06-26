//! Lightweight PocketIC smoke checks for the local developer loop.
//!
//! Keep this target intentionally small: one PocketIC server, one two-shard
//! federation install, and a representative Router -> Index -> Graph query.
//! Detailed failure-mode, upgrade, timer, recovery, and vector coverage stays
//! in the full integration suite.

use gleaph_pocket_ic_tests::{
    admin_intern_property, create_vertex_property_index, e2e_insert_vertex_with_property,
    gql_query_as_admin, install_federation, router_check_registry_invariants,
};

#[test]
fn smoke_router_registry_index_and_gql_dispatch() {
    let env = install_federation();

    router_check_registry_invariants(&env).expect("fresh two-shard registry must be consistent");

    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        "smoke_vertex_age",
        "Person",
        "age",
        "smoke_router_registry_index_and_gql_dispatch_create_index",
    );
    let _ = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);

    let result = gql_query_as_admin(&env, "MATCH (n:Person {age: 5}) RETURN n");
    assert_eq!(
        result.row_count, 1,
        "indexed GQL dispatch should find the seeded vertex"
    );
}
