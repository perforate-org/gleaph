//! PocketIC: ADR 0030 slice 9 — public `DROP CONSTRAINT` lifecycle (Active → Dropping → Removed).
//!
//! `DROP CONSTRAINT` synchronously flips the constraint `Active → Dropping` and returns; the
//! drop-drain recovery lane (Driver 3) then drains every reservation and pending unique effect keyed
//! by the dropped `ConstraintNameId`, reusing Drivers 1/2, and only then deletes the record
//! (`Removed`). These tests exercise the public ingress (Candid → `gql_execute_idempotent`) and the
//! full lifecycle invariants:
//!   - committed values are freed and the constraint stops enforcing once `Dropping`;
//!   - new DML proceeds **unconstrained** while `Dropping` (no `UniquenessViolation`), but a
//!     same-name re-CREATE is rejected (`Conflict`) until `Removed`;
//!   - the completion gate keeps the constraint `Dropping` (re-CREATE blocked) while any pinned
//!     effect for the dropped id remains — "no reservations only" is insufficient;
//!   - the lifecycle survives a canister upgrade and does not disable unrelated constraints.

use candid::Encode;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    FederationEnv, arm_graph_unique_ack_fault_all_shards, drain_maintenance_via_timer,
    gql_execute_idempotent_as_admin, gql_execute_idempotent_as_admin_expect_err,
    gql_execute_idempotent_result_as_admin, gql_query_as_admin, graph_unique_outbox_len_all_shards,
    install_federation, install_single_shard_federation, run_router_recovery_after_reservation_ttl,
    run_router_recovery_timer, start_graph_shard, stop_graph_shard, wasm_bytes,
};

const CONSTRAINT: &str = "user_email";
const LABEL: &str = "User";
const OTHER_LABEL: &str = "Member";
const PROPERTY: &str = "email";

const GRAPH_FAULT_NONE: u8 = 0;
const GRAPH_FAULT_TRAP_ON_UNIQUE_ACK: u8 = 1;

fn create_for(name: &str, label: &str, property: &str) -> String {
    format!("CREATE CONSTRAINT {name} FOR (n:{label}) REQUIRE n.{property} IS UNIQUE")
}

fn drop_for(name: &str) -> String {
    format!("DROP CONSTRAINT {name}")
}

fn insert(label: &str, property: &str, value: &str) -> String {
    format!("INSERT (:{label} {{{property}: '{value}'}})")
}

fn live_count(env: &FederationEnv, label: &str) -> u64 {
    gql_query_as_admin(env, &format!("MATCH (n:{label}) RETURN n")).row_count
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

/// DROP frees the constraint's committed values and stops enforcement: once `Dropping`, a previously
/// rejected duplicate `INSERT` now commits unconstrained, and after the drain completes the value is
/// fully free.
#[test]
fn drop_constraint_releases_committed_values() {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(
        &env,
        &create_for(CONSTRAINT, LABEL, PROPERTY),
        "s9-rel-create",
    );
    gql_execute_idempotent_as_admin(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s9-rel-ins",
    );

    // Enforced while Active: a same-value duplicate is rejected non-retryably.
    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s9-rel-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "{dup:?}"
    );

    // DROP flips the constraint to Dropping and returns immediately.
    let dropped =
        gql_execute_idempotent_result_as_admin(&env, &drop_for(CONSTRAINT), "s9-rel-drop");
    assert_eq!(dropped.row_count, 0, "DROP CONSTRAINT reports no rows");

    // Recovery drains the committed reservation and removes the record.
    run_router_recovery_timer(&env);

    // The value is now free: the same value inserts again (unconstrained), and a further duplicate
    // is likewise admitted — the constraint no longer enforces.
    gql_execute_idempotent_as_admin(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s9-rel-reuse",
    );
    gql_execute_idempotent_as_admin(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s9-rel-reuse2",
    );
    drain_maintenance_via_timer(&env, env.graph_source);
    assert_eq!(
        live_count(&env, LABEL),
        3,
        "after DROP the value is unconstrained: all three inserts of a@example.com committed"
    );
}

/// While `Dropping`, new constrained DML proceeds **unconstrained** (no `UniquenessViolation`), but a
/// same-name re-CREATE is rejected (`Conflict`) until the drain completes (`Removed`); afterward the
/// name is reusable on a brand-new label.
#[test]
fn drop_then_recreate_same_name_different_label() {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(
        &env,
        &create_for(CONSTRAINT, LABEL, PROPERTY),
        "s9-rc-create",
    );
    gql_execute_idempotent_as_admin(&env, &insert(LABEL, PROPERTY, "a@example.com"), "s9-rc-ins");

    gql_execute_idempotent_as_admin(&env, &drop_for(CONSTRAINT), "s9-rc-drop");

    // Immediately after DROP (still Dropping): the same name is a tombstone — re-CREATE rejected.
    let blocked = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &create_for(CONSTRAINT, OTHER_LABEL, "code"),
        "s9-rc-recreate-blocked",
    );
    assert!(
        matches!(blocked, RouterError::Conflict(_)),
        "same-name re-CREATE while Dropping must Conflict, got {blocked:?}"
    );

    // Drain to Removed.
    run_router_recovery_timer(&env);

    // The name is now reusable on a brand-new label, and the fresh constraint enforces.
    let recreated = gql_execute_idempotent_result_as_admin(
        &env,
        &create_for(CONSTRAINT, OTHER_LABEL, "code"),
        "s9-rc-recreate-ok",
    );
    assert_eq!(recreated.row_count, 0, "re-CREATE after Removed succeeds");
    gql_execute_idempotent_as_admin(&env, &insert(OTHER_LABEL, "code", "x"), "s9-rc-new-ins");
    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert(OTHER_LABEL, "code", "x"),
        "s9-rc-new-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "the re-created constraint enforces independently, got {dup:?}"
    );
}

/// A new `INSERT` against a `Dropping` constraint on a previously-constrained value succeeds
/// **unconstrained** (the constraint is absent for new acquires), while a same-name re-CREATE stays
/// rejected — the two halves of the slice-9 contract ("Dropping = absent for acquires" vs "tombstone
/// blocks re-CREATE").
#[test]
fn dropping_constraint_admits_new_inserts_but_blocks_recreate() {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(
        &env,
        &create_for(CONSTRAINT, LABEL, PROPERTY),
        "s9-pt-create",
    );
    gql_execute_idempotent_as_admin(&env, &insert(LABEL, PROPERTY, "a@example.com"), "s9-pt-ins");

    // Flip to Dropping but do **not** run recovery yet, so the constraint is still mid-drain.
    gql_execute_idempotent_as_admin(&env, &drop_for(CONSTRAINT), "s9-pt-drop");

    // (a) A new INSERT of the previously-constrained value succeeds unconstrained (no violation):
    // it does not raise a `UniquenessViolation`, and a second a@example.com vertex now exists.
    gql_execute_idempotent_as_admin(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s9-pt-passthrough",
    );
    assert_eq!(
        gql_query_as_admin(&env, "MATCH (n) RETURN n").row_count,
        2,
        "INSERT proceeds unconstrained while Dropping: both a@example.com vertices exist"
    );

    // (b) Same-name re-CREATE is still rejected (tombstone) until Removed.
    let blocked = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &create_for(CONSTRAINT, OTHER_LABEL, "code"),
        "s9-pt-recreate-blocked",
    );
    assert!(
        matches!(blocked, RouterError::Conflict(_)),
        "same-name re-CREATE while Dropping must Conflict, got {blocked:?}"
    );
}

/// A canonical-write failure (stopped shard) leaves the value `Reserved`; a subsequent DROP must
/// still drain that held reservation once the shard is reachable, leaving no stale committed
/// reservation, and the name becomes reusable.
#[test]
fn drop_during_in_flight_insert() {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(
        &env,
        &create_for(CONSTRAINT, LABEL, PROPERTY),
        "s9-if-create",
    );

    // Stop the shard so the no-await Try persists the reservation but the canonical dispatch fails.
    stop_graph_shard(&env, env.graph_source);
    let _ = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert(LABEL, PROPERTY, "held@example.com"),
        "s9-if-held",
    );

    // DROP while the reservation is held; then make the shard reachable and drive recovery.
    gql_execute_idempotent_as_admin(&env, &drop_for(CONSTRAINT), "s9-if-drop");
    start_graph_shard(&env, env.graph_source);
    run_router_recovery_after_reservation_ttl(&env);

    // The drain cancelled the uncommitted reservation; the name is reusable on a brand-new label.
    let recreated = gql_execute_idempotent_result_as_admin(
        &env,
        &create_for(CONSTRAINT, OTHER_LABEL, "code"),
        "s9-if-recreate",
    );
    assert_eq!(
        recreated.row_count, 0,
        "after the held reservation drained, the dropped name is reusable"
    );
}

/// The completion gate is stronger than "no reservations": while a pinned `Acquire` effect for the
/// dropped id remains (the graph trapped the Router's ack), the constraint stays `Dropping` and a
/// same-name re-CREATE is rejected. Only after the effect is acked (drained) does the drop complete
/// and the name become reusable.
#[test]
fn recreate_blocked_until_pending_effect_drained() {
    // Two shards (`install_federation`) so the constraint freezes to FederatedTcc and the pinned
    // `Acquire` outbox effect this gate depends on exists; ADR 0030 slice 10's single-shard
    // `ShardLocalGlobal` fast path keeps no reservation/outbox effect at all.
    let env = install_federation();
    gql_execute_idempotent_as_admin(
        &env,
        &create_for(CONSTRAINT, LABEL, PROPERTY),
        "s9-gate-create",
    );

    // Pin the Acquire: the owning shard traps the Router's ack, so Confirm commits the reservation
    // (pending_acquire_ack set) but the effect stays pinned in the outbox.
    arm_graph_unique_ack_fault_all_shards(&env, GRAPH_FAULT_TRAP_ON_UNIQUE_ACK);
    gql_execute_idempotent_as_admin(
        &env,
        &insert(LABEL, PROPERTY, "g@example.com"),
        "s9-gate-ins",
    );
    assert_eq!(
        graph_unique_outbox_len_all_shards(&env),
        1,
        "the Acquire is pinned because the Router's ack trapped"
    );

    // DROP, then drive recovery **with the ack still trapping**: the pinned effect (and its
    // pending-ack Committed reservation) keep the constraint Dropping, so re-CREATE stays rejected.
    gql_execute_idempotent_as_admin(&env, &drop_for(CONSTRAINT), "s9-gate-drop");
    run_router_recovery_timer(&env);
    let blocked = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &create_for(CONSTRAINT, OTHER_LABEL, "code"),
        "s9-gate-blocked",
    );
    assert!(
        matches!(blocked, RouterError::Conflict(_)),
        "a pinned effect for the dropped id keeps it Dropping (gate > 'no reservations'), got {blocked:?}"
    );

    // Clear the fault and drive recovery: the pinned Acquire is acked, the reservation purged, and
    // the gate finally holds — the record is Removed and the name reusable.
    arm_graph_unique_ack_fault_all_shards(&env, GRAPH_FAULT_NONE);
    run_router_recovery_after_reservation_ttl(&env);
    assert_eq!(
        graph_unique_outbox_len_all_shards(&env),
        0,
        "recovery acked the pinned Acquire"
    );
    let recreated = gql_execute_idempotent_result_as_admin(
        &env,
        &create_for(CONSTRAINT, OTHER_LABEL, "code"),
        "s9-gate-recreate",
    );
    assert_eq!(
        recreated.row_count, 0,
        "once the pending effect drained, the dropped name is reusable"
    );
}

/// The `Dropping`/`Removed` lifecycle survives a canister upgrade: a DROP issued before an upgrade is
/// completed by the re-armed recovery timer afterward, and the name becomes reusable.
#[test]
fn drop_survives_upgrade() {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(
        &env,
        &create_for(CONSTRAINT, LABEL, PROPERTY),
        "s9-up-create",
    );
    gql_execute_idempotent_as_admin(&env, &insert(LABEL, PROPERTY, "a@example.com"), "s9-up-ins");

    gql_execute_idempotent_as_admin(&env, &drop_for(CONSTRAINT), "s9-up-drop");
    // Upgrade before the drain completes: the Dropping record + drop_scan_generation must survive,
    // and the recovery timer must re-arm in post_upgrade.
    upgrade_federation(&env);
    run_router_recovery_timer(&env);

    let recreated = gql_execute_idempotent_result_as_admin(
        &env,
        &create_for(CONSTRAINT, OTHER_LABEL, "code"),
        "s9-up-recreate",
    );
    assert_eq!(
        recreated.row_count, 0,
        "the drop lifecycle converged to Removed across the upgrade"
    );
}

/// Dropping one constraint must not disable an unrelated one: the dropped constraint stops enforcing
/// (duplicates admitted) while the surviving constraint keeps rejecting duplicates.
#[test]
fn drop_does_not_disable_unrelated_constraints() {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(
        &env,
        &create_for(CONSTRAINT, LABEL, PROPERTY),
        "s9-un-create-1",
    );
    gql_execute_idempotent_as_admin(
        &env,
        &create_for("member_code", OTHER_LABEL, "code"),
        "s9-un-create-2",
    );
    gql_execute_idempotent_as_admin(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s9-un-ins-1",
    );
    gql_execute_idempotent_as_admin(&env, &insert(OTHER_LABEL, "code", "m1"), "s9-un-ins-2");

    // Drop only the user_email constraint.
    gql_execute_idempotent_as_admin(&env, &drop_for(CONSTRAINT), "s9-un-drop");
    run_router_recovery_timer(&env);

    // The dropped constraint no longer enforces: a duplicate User.email is admitted.
    gql_execute_idempotent_as_admin(
        &env,
        &insert(LABEL, PROPERTY, "a@example.com"),
        "s9-un-dropped-dup",
    );
    drain_maintenance_via_timer(&env, env.graph_source);
    assert_eq!(
        live_count(&env, LABEL),
        2,
        "the dropped constraint admits duplicates"
    );

    // The unrelated constraint still enforces: a duplicate Member.code is rejected.
    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert(OTHER_LABEL, "code", "m1"),
        "s9-un-survivor-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "the surviving constraint still enforces, got {dup:?}"
    );
}
