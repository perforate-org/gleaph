//! PocketIC: ADR 0030 — cross-shard uniqueness write-path lifecycle (slice 7 gate).
//!
//! Exercises the *enforced* lifecycle end to end through the real ingress seam
//! (Candid → `gql_execute_idempotent`), not just the store/parser units:
//!   - a single-vertex `INSERT` under a declared constraint reserves (Try), commits canonically
//!     (the shard pins an `Acquire`), and is Confirmed inline — and is then queryable;
//!   - a second `INSERT` of the same value is refused **non-retryably** (`UniquenessViolation`);
//!   - a value held `Reserved` by an in-flight mutation refuses a competing claim **retryably**
//!     (`UniquenessReservationInFlight`);
//!   - `SET` on a constrained property is deferred (`NotImplemented`) until the two-phase protocol
//!     covers it — never admitted unguarded;
//!   - a `DELETE` releases the reservation so the value becomes reusable.
//!
//! `CREATE CONSTRAINT` DDL stays publicly `NotImplemented`; the constraint is declared through the
//! `pocket-ic-e2e` test seam `test_declare_unique_constraint` (admin-authorized, declare-on-empty).

use candid::Principal;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    GRAPH_NAME, gql_execute_idempotent_as_admin, gql_execute_idempotent_as_admin_expect_err,
    gql_query_as_admin, install_single_shard_federation, run_router_recovery_timer,
    start_graph_shard, stop_graph_shard, test_declare_unique_constraint,
    test_declare_unique_constraint_as,
};

const CONSTRAINT: &str = "acct_email";
const LABEL: &str = "Account";
const PROPERTY: &str = "email";

fn insert_account(email: &str) -> String {
    format!("INSERT (:{LABEL} {{{PROPERTY}: '{email}'}})")
}

/// The constraint-declaration seam is admin-gated: a non-admin caller is rejected with
/// `NotAuthorized` and no constraint is created (a subsequent same-value INSERT pair both commit).
#[test]
fn declare_unique_constraint_rejects_non_admin() {
    let env = install_single_shard_federation();
    let intruder = Principal::from_slice(&[0x11; 29]);

    let err =
        test_declare_unique_constraint_as(&env, intruder, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY)
            .expect_err("a non-admin caller must not declare a constraint");
    assert!(
        matches!(err, RouterError::NotAuthorized),
        "non-admin constraint declaration must be NotAuthorized, got {err:?}"
    );

    // The guard ran before any store mutation: no constraint exists, so duplicate values both commit.
    gql_execute_idempotent_as_admin(&env, &insert_account("z@example.com"), "noguard-1");
    gql_execute_idempotent_as_admin(&env, &insert_account("z@example.com"), "noguard-2");
    let live = gql_query_as_admin(&env, &format!("MATCH (n:{LABEL}) RETURN n"));
    assert_eq!(
        live.row_count, 2,
        "with no constraint declared, duplicate values both commit"
    );
}

/// A constrained `INSERT` commits and becomes queryable; a same-value `INSERT` under a different
/// client key is refused non-retryably.
#[test]
fn constrained_insert_commits_and_rejects_duplicate() {
    let env = install_single_shard_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    // A federated INSERT is RETURN-less: it reports row_count 0; the commit is verified by a MATCH.
    gql_execute_idempotent_as_admin(&env, &insert_account("a@example.com"), "ins-1");

    let live = gql_query_as_admin(&env, &format!("MATCH (n:{LABEL}) RETURN n"));
    assert_eq!(live.row_count, 1, "the committed vertex is visible");

    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_account("a@example.com"),
        "ins-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "a committed-value duplicate must be a non-retryable UniquenessViolation, got {dup:?}"
    );

    let still_live = gql_query_as_admin(&env, &format!("MATCH (n:{LABEL}) RETURN n"));
    assert_eq!(
        still_live.row_count, 1,
        "the rejected duplicate created no vertex"
    );

    // A distinct value under the same constraint is admitted (two vertices now live).
    gql_execute_idempotent_as_admin(&env, &insert_account("b@example.com"), "ins-2");
    let both = gql_query_as_admin(&env, &format!("MATCH (n:{LABEL}) RETURN n"));
    assert_eq!(
        both.row_count, 2,
        "a distinct value is independently reservable"
    );
}

/// A value held `Reserved` by an in-flight mutation (its canonical dispatch stalled on a stopped
/// shard, so the reservation persists without committing) refuses a competing same-value claim
/// **retryably** — the loser must retry after the in-flight saga resolves, not be told the value is
/// permanently taken.
#[test]
fn in_flight_reservation_refuses_competitor_retryably() {
    let env = install_single_shard_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    // Stall the canonical write after Try: the reservation co-commits with the envelope (no-await),
    // then the dispatch to the stopped shard fails, leaving the value `Reserved` by a live mutation.
    stop_graph_shard(&env, env.graph_source);
    let stalled = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_account("c@example.com"),
        "inflight-winner",
    );
    // The exact variant is the dispatch failure; what matters is the reservation now exists.
    let _ = stalled;

    let competitor = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_account("c@example.com"),
        "inflight-loser",
    );
    assert!(
        matches!(competitor, RouterError::UniquenessReservationInFlight(_)),
        "a value Reserved by an in-flight mutation must refuse competitors retryably, got {competitor:?}"
    );

    // Cleanup so the harness teardown leaves a reachable shard.
    start_graph_shard(&env, env.graph_source);
}

/// `SET` on a constrained property is deferred (`NotImplemented`) until the acquire/release protocol
/// covers update-in-place — it must never reach the canonical write unguarded.
#[test]
fn set_on_constrained_property_is_deferred() {
    let env = install_single_shard_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    gql_execute_idempotent_as_admin(&env, &insert_account("d@example.com"), "set-seed");

    let err = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &format!("MATCH (n:{LABEL}) SET n.{PROPERTY} = 'e@example.com'"),
        "set-attempt",
    );
    assert!(
        matches!(err, RouterError::NotImplemented(_)),
        "SET on a constrained property must be deferred NotImplemented, got {err:?}"
    );
}

/// A `DELETE` of a constrained vertex releases its reservation so the freed value is reusable.
#[test]
fn delete_releases_reservation_value_is_reusable() {
    let env = install_single_shard_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    gql_execute_idempotent_as_admin(&env, &insert_account("f@example.com"), "rel-ins");

    // Re-using the value before release is rejected.
    let blocked = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_account("f@example.com"),
        "rel-dup",
    );
    assert!(
        matches!(blocked, RouterError::UniquenessViolation(_)),
        "value is still reserved before delete, got {blocked:?}"
    );

    gql_execute_idempotent_as_admin(
        &env,
        &format!("MATCH (n:{LABEL}) DETACH DELETE n"),
        "rel-del",
    );
    let gone = gql_query_as_admin(&env, &format!("MATCH (n:{LABEL}) RETURN n"));
    assert_eq!(gone.row_count, 0, "the constrained vertex is deleted");

    // Drain any held Release effect (the happy-path reconcile is inline, this is belt-and-braces).
    run_router_recovery_timer(&env);

    gql_execute_idempotent_as_admin(&env, &insert_account("f@example.com"), "rel-reuse");
    let reused = gql_query_as_admin(&env, &format!("MATCH (n:{LABEL}) RETURN n"));
    assert_eq!(
        reused.row_count, 1,
        "the released value is reusable after the constrained delete"
    );
}
