//! PocketIC: ADR 0030 slice 10 — `ShardLocalGlobal` UNIQUE fast path (single-shard graphs).
//!
//! A UNIQUE constraint created on a graph with exactly one live shard is frozen to the
//! `ShardLocalGlobal` strategy and enforced entirely inside that owning shard's local unique table
//! (`GRAPH_LOCAL_UNIQUE_VALUES`), bypassing the federated Router reservation / unique-effect outbox
//! path while keeping graph-wide semantics. These tests drive the real public ingress (Candid →
//! `gql_execute_idempotent` / `gql_query`) and prove the end-to-end fast-path behaviour the
//! library unit tests cannot:
//!   - a constrained INSERT is enforced and a same-value duplicate is rejected;
//!   - a DELETE frees the value by owner match, so the same value inserts again (and re-duplicates);
//!   - `DROP CONSTRAINT` is held `Dropping` until the owning shard's local table is drained, then the
//!     name becomes reusable (the slice-10 drain branch gates `Removed` on local-table empty);
//!   - the local table is canonical stable memory: enforcement and release survive a canister upgrade.

use candid::Encode;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    FederationEnv, drain_maintenance_via_timer, gql_execute_idempotent_as_admin,
    gql_execute_idempotent_as_admin_expect_err, gql_execute_idempotent_result_as_admin,
    gql_query_as_admin, install_single_shard_federation, run_router_recovery_timer, wasm_bytes,
};

const CONSTRAINT: &str = "user_email";
const LABEL: &str = "User";
const OTHER_LABEL: &str = "Member";
const PROPERTY: &str = "email";

fn create_for(name: &str, label: &str, property: &str) -> String {
    format!("CREATE CONSTRAINT {name} FOR (n:{label}) REQUIRE n.{property} IS UNIQUE")
}

fn insert(label: &str, property: &str, value: &str) -> String {
    format!("INSERT (:{label} {{{property}: '{value}'}})")
}

fn delete(label: &str, property: &str, value: &str) -> String {
    format!("MATCH (n:{label}) WHERE n.{property} = '{value}' DETACH DELETE n")
}

fn drop_for(name: &str) -> String {
    format!("DROP CONSTRAINT {name}")
}

fn upgrade_federation(env: &FederationEnv) {
    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.router, wasm_bytes("ROUTER_WASM"), empty.clone(), None)
        .expect("upgrade router");
    env.pic
        .upgrade_canister(env.index, wasm_bytes("INDEX_WASM"), empty.clone(), None)
        .expect("upgrade index");
    env.pic
        .upgrade_canister(env.graph_source, wasm_bytes("GRAPH_WASM"), empty, None)
        .expect("upgrade graph shard");
}

/// A `ShardLocalGlobal` constraint (single-shard CREATE) is enforced through the owning shard's local
/// table, and a DELETE frees the value by owner match so the same value is insertable again.
#[test]
fn shard_local_global_enforces_and_frees_on_delete() {
    let env = install_single_shard_federation();

    gql_execute_idempotent_as_admin(
        &env,
        &create_for(CONSTRAINT, LABEL, PROPERTY),
        "s10-en-create",
    );
    gql_execute_idempotent_as_admin(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s10-en-ins",
    );
    drain_maintenance_via_timer(&env, env.graph_source);
    assert_eq!(
        gql_query_as_admin(&env, &format!("MATCH (n:{LABEL}) RETURN n")).row_count,
        1,
        "the constrained vertex committed"
    );

    // The local fast path enforces graph-wide uniqueness: a same-value duplicate is rejected.
    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s10-en-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "ShardLocalGlobal duplicate must be UniquenessViolation, got {dup:?}"
    );

    // Deleting the owner frees its value in the local table (owner-matched release).
    gql_execute_idempotent_as_admin(
        &env,
        &delete(LABEL, PROPERTY, "a@example.com"),
        "s10-en-del",
    );
    drain_maintenance_via_timer(&env, env.graph_source);
    assert_eq!(
        gql_query_as_admin(&env, &format!("MATCH (n:{LABEL}) RETURN n")).row_count,
        0,
        "the constrained vertex was deleted"
    );

    // The freed value is reusable, and re-claiming it re-arms enforcement.
    gql_execute_idempotent_as_admin(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s10-en-reins",
    );
    drain_maintenance_via_timer(&env, env.graph_source);
    let redup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s10-en-redup",
    );
    assert!(
        matches!(redup, RouterError::UniquenessViolation(_)),
        "the re-inserted value is enforced again, got {redup:?}"
    );
}

/// `DROP CONSTRAINT` on a `ShardLocalGlobal` constraint stays `Dropping` (same-name re-CREATE
/// rejected) until the recovery drain purges the owning shard's local table empty; afterward the name
/// is reusable. This exercises the slice-10 drain branch that gates `Removed` on local-table empty.
#[test]
fn shard_local_global_drop_drains_local_table_and_allows_recreate() {
    let env = install_single_shard_federation();

    gql_execute_idempotent_as_admin(
        &env,
        &create_for(CONSTRAINT, LABEL, PROPERTY),
        "s10-dr-create",
    );
    gql_execute_idempotent_as_admin(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s10-dr-ins",
    );

    gql_execute_idempotent_as_admin(&env, &drop_for(CONSTRAINT), "s10-dr-drop");

    // Before the drain completes the tombstone holds: re-CREATE is rejected (Dropping).
    let blocked = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &create_for(CONSTRAINT, OTHER_LABEL, "code"),
        "s10-dr-recreate-blocked",
    );
    assert!(
        matches!(blocked, RouterError::Conflict(_)),
        "re-CREATE must be blocked until the local table is drained, got {blocked:?}"
    );

    // The drain purges the owning shard's local table; once empty, the record reaches Removed.
    run_router_recovery_timer(&env);

    let recreated = gql_execute_idempotent_result_as_admin(
        &env,
        &create_for(CONSTRAINT, OTHER_LABEL, "code"),
        "s10-dr-recreate-ok",
    );
    assert_eq!(
        recreated.row_count, 0,
        "after the local table drained, the dropped name is reusable"
    );

    // The re-created constraint enforces independently.
    gql_execute_idempotent_as_admin(&env, &insert(OTHER_LABEL, "code", "x"), "s10-dr-new-ins");
    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert(OTHER_LABEL, "code", "x"),
        "s10-dr-new-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "the re-created ShardLocalGlobal constraint enforces, got {dup:?}"
    );
}

/// The local unique table is canonical stable memory: a `ShardLocalGlobal` constraint keeps enforcing
/// across a canister upgrade, and owner-matched release still works afterward.
#[test]
fn shard_local_global_survives_upgrade() {
    let env = install_single_shard_federation();

    gql_execute_idempotent_as_admin(
        &env,
        &create_for(CONSTRAINT, LABEL, PROPERTY),
        "s10-up-create",
    );
    gql_execute_idempotent_as_admin(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s10-up-ins",
    );
    drain_maintenance_via_timer(&env, env.graph_source);

    upgrade_federation(&env);

    // The local value survived the upgrade: a duplicate is still rejected.
    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s10-up-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "the ShardLocalGlobal local table must persist across upgrade, got {dup:?}"
    );

    // Release still works post-upgrade: delete frees the value, which is then insertable again.
    gql_execute_idempotent_as_admin(
        &env,
        &delete(LABEL, PROPERTY, "a@example.com"),
        "s10-up-del",
    );
    drain_maintenance_via_timer(&env, env.graph_source);
    gql_execute_idempotent_as_admin(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s10-up-reins",
    );
    let redup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s10-up-redup",
    );
    assert!(
        matches!(redup, RouterError::UniquenessViolation(_)),
        "post-upgrade release + re-claim re-arms enforcement, got {redup:?}"
    );
}
