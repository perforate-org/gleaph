//! PocketIC: ADR 0030 — cross-shard uniqueness failure-injection & recovery (slice 7 gate).
//!
//! Injects a canonical-write failure *after* the no-await Try (by stopping the target shard so the
//! reservation co-commits with the envelope but the dispatch fails), then drives convergence via the
//! two sanctioned routes:
//!   - **roll-forward** — an idempotent client retry under the same key re-dispatches, commits the
//!     canonical write, and Confirms the held reservation (exercising idempotent Confirm);
//!   - **reclaim** — when the saga is abandoned, the autonomous reclaim driver (Driver 1) reads the
//!     replicated proof, sees the `Acquire` absent on the reachable shard, terminally fails the
//!     uncommitted dispatch and Cancels the reservation, freeing the value;
//!   - **upgrade-reopen** — the same reclaim convergence holds across a canister upgrade, proving the
//!     reservation/envelope survive `post_upgrade` and the recovery timer re-arms.
//!
//! The constraint is declared via the `pocket-ic-e2e` seam (`CREATE CONSTRAINT` stays publicly
//! `NotImplemented`).

use candid::{Decode, Encode};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, gql_execute_idempotent_as_admin,
    gql_execute_idempotent_as_admin_expect_err, gql_query_as_admin,
    install_single_shard_federation, run_router_recovery_after_reservation_ttl, start_graph_shard,
    stop_graph_shard, test_declare_unique_constraint, wasm_bytes,
};

const CONSTRAINT: &str = "acct_email";
const LABEL: &str = "Account";
const PROPERTY: &str = "email";

fn insert_account(email: &str) -> String {
    format!("INSERT (:{LABEL} {{{PROPERTY}: '{email}'}})")
}

/// `gql_execute_idempotent` as admin, returning the full `Result` so callers can assert either
/// outcome (a roll-forward retry succeeds; an abandoned key keeps returning its terminal error).
fn gql_execute_idempotent_result(
    env: &FederationEnv,
    query: &str,
    client_mutation_key: &str,
) -> Result<GqlQueryResult, RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "gql_execute_idempotent",
            Encode!(
                &query.to_string(),
                &Vec::<u8>::new(),
                &client_mutation_key.to_string()
            )
            .expect("encode gql_execute_idempotent"),
        )
        .unwrap_or_else(|e| panic!("gql_execute_idempotent on router: {e:?}"));
    Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_execute_idempotent")
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

/// Inject a held reservation: stop the shard so the no-await Try persists the reservation but the
/// canonical dispatch fails. Returns once the failed insert has left the value `Reserved`.
fn inject_held_reservation(env: &FederationEnv, email: &str, key: &str) {
    stop_graph_shard(env, env.graph_source);
    let err = gql_execute_idempotent_as_admin_expect_err(env, &insert_account(email), key);
    // The dispatch to a stopped shard fails; the reservation is now held pending recovery.
    let _ = err;
}

/// A canonical-write failure after Try is recovered by an idempotent client retry: the same key
/// re-dispatches once the shard is reachable, commits, and Confirms the held reservation. The value
/// is then committed (a fresh same-value claim is refused non-retryably).
#[test]
fn held_dispatch_recovered_by_idempotent_retry() {
    let env = install_single_shard_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    inject_held_reservation(&env, "g@example.com", "rollfwd");

    // Shard reachable again — the idempotent retry rolls the same saga forward to a commit.
    start_graph_shard(&env, env.graph_source);
    let retried = gql_execute_idempotent_result(&env, &insert_account("g@example.com"), "rollfwd");
    retried.expect("idempotent retry rolls the held reservation forward to a commit");

    let live = gql_query_as_admin(&env, &format!("MATCH (n:{LABEL}) RETURN n"));
    assert_eq!(live.row_count, 1, "the recovered vertex is committed");

    // The reservation is now Committed: a fresh same-value claim is non-retryable.
    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_account("g@example.com"),
        "rollfwd-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "after roll-forward the value is committed, got {dup:?}"
    );
}

/// An abandoned held reservation is reclaimed: with the shard reachable and the `Acquire` proven
/// absent, the reclaim driver terminally fails the uncommitted dispatch and Cancels the reservation
/// past the TTL window. The freed value is reusable under a new key; the abandoned key stays
/// terminally failed (its stored error is replayed, never re-dispatched).
#[test]
fn abandoned_reservation_reclaimed_after_ttl() {
    let env = install_single_shard_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    inject_held_reservation(&env, "h@example.com", "abandoned");

    // Make the proof scope reachable, then let the reclaim driver run past the reservation TTL.
    start_graph_shard(&env, env.graph_source);
    run_router_recovery_after_reservation_ttl(&env);

    // The reclaimed value is reusable under a fresh key (verified by the live count below).
    gql_execute_idempotent_as_admin(&env, &insert_account("h@example.com"), "reclaimed");

    // The abandoned key is terminally failed and is never re-dispatched (no duplicate vertex).
    let replay = gql_execute_idempotent_result(&env, &insert_account("h@example.com"), "abandoned");
    assert!(
        replay.is_err(),
        "the terminally-failed key replays its stored error, got {replay:?}"
    );
    let live = gql_query_as_admin(&env, &format!("MATCH (n:{LABEL}) RETURN n"));
    assert_eq!(
        live.row_count, 1,
        "only the fresh-key vertex exists; the abandoned saga never committed"
    );
}

/// The reclaim convergence survives a canister upgrade: the held reservation and dispatch envelope
/// persist across `post_upgrade`, the recovery timer re-arms, and the reservation is reclaimed past
/// the TTL — freeing the value.
#[test]
fn upgrade_reopen_reconciles_held_reservation() {
    let env = install_single_shard_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    inject_held_reservation(&env, "i@example.com", "upgrade-held");

    // Upgrade every canister mid-saga; the reservation/envelope live in stable memory and the
    // router's post_upgrade re-arms the recovery driver.
    start_graph_shard(&env, env.graph_source);
    upgrade_federation(&env);

    run_router_recovery_after_reservation_ttl(&env);

    gql_execute_idempotent_as_admin(&env, &insert_account("i@example.com"), "upgrade-reclaimed");
    let live = gql_query_as_admin(&env, &format!("MATCH (n:{LABEL}) RETURN n"));
    assert_eq!(
        live.row_count, 1,
        "post-upgrade recovery reclaimed the held reservation, freeing the value"
    );
}
