//! PocketIC: ADR 0030 — cross-shard uniqueness *trap* failure-injection (slice 7 gate).
//!
//! These complement `adr0030_uniqueness_recovery.rs` (which injects *dispatch* failures by stopping a
//! shard) with true **trap** boundaries, armed via the `pocket-ic-e2e` seam `test_arm_fault`:
//!   - **Try-then-Router-trap** — a trap after the no-`await` Try (before the first dispatch `await`)
//!     rolls the reservation/envelope back with the message, leaving the value free;
//!   - **Confirm-then-trap** — a trap in the post-dispatch callback before Confirm leaves the shard's
//!     canonical write + pinned `Acquire` durable but the reservation `Reserved`; recovery (reading
//!     the `Acquire` present) Confirms it — the reservation is re-confirmable, never lost;
//!   - **true concurrent same-value conflict** — two ingress messages racing the same value: one
//!     wins, and the loser is retryable or a committed duplicate.
//!
//! `CREATE CONSTRAINT` stays publicly `NotImplemented`; the constraint is declared through the
//! `pocket-ic-e2e` seam `test_declare_unique_constraint`.
//!
//! These run on a two-shard federation (`install_federation`): ADR 0030 slice 10 freezes a constraint
//! created on a single-shard graph to the `ShardLocalGlobal` fast path, which bypasses the federated
//! reservation / outbox / ack machinery these traps target. A constrained value routes by hash to one
//! of the two shards, so the fault/outbox/journal probes act on both shards to stay value-agnostic.

use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    GRAPH_NAME, admin_sweep_mutation_keys, advance_past_journal_eviction,
    arm_graph_unique_ack_fault_all_shards, arm_router_fault, drain_maintenance_via_timer,
    evict_graph_mutation_journal_all_shards, gql_execute_idempotent_as_admin,
    gql_execute_idempotent_as_admin_expect_err, gql_execute_idempotent_as_admin_expect_trap,
    gql_execute_idempotent_pair_concurrent_as_admin, graph_mutation_journal_len_all_shards,
    graph_unique_outbox_len_all_shards, install_federation,
    run_router_recovery_after_reservation_ttl, start_graph_shards_all, stop_graph_shards_all,
    test_declare_unique_constraint, test_force_reclaiming,
};

const CONSTRAINT: &str = "acct_email";
const LABEL: &str = "Account";
const PROPERTY: &str = "email";

const FAULT_NONE: u8 = 0;
const FAULT_TRAP_AFTER_TRY: u8 = 1;
const FAULT_TRAP_BEFORE_CONFIRM: u8 = 2;

// Graph-shard fault codes (`crate::test_fault` on `gleaph_graph`).
const GRAPH_FAULT_NONE: u8 = 0;
const GRAPH_FAULT_TRAP_ON_UNIQUE_ACK: u8 = 1;

fn insert_account(email: &str) -> String {
    format!("INSERT (:{LABEL} {{{PROPERTY}: '{email}'}})")
}

/// A Router trap *after* the no-`await` Try (before the first dispatch `await`) rolls the reservation
/// and envelope back with the message: the value stays free, so a later insert of the same value
/// commits, and the trapped key left no stranded reservation.
#[test]
fn try_then_router_trap_rolls_back_reservation() {
    let env = install_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    arm_router_fault(&env, FAULT_TRAP_AFTER_TRY);
    gql_execute_idempotent_as_admin_expect_trap(&env, &insert_account("t@example.com"), "try-trap");
    // Clear the fault in its own committed message (the trap rolled back only the trapping message).
    arm_router_fault(&env, FAULT_NONE);

    // The value is free: a fresh insert of the same value under a new key commits.
    gql_execute_idempotent_as_admin(&env, &insert_account("t@example.com"), "try-trap-reuse");

    // And it is now genuinely reserved: a same-value duplicate is non-retryable.
    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_account("t@example.com"),
        "try-trap-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "after the successful reuse the value is committed, got {dup:?}"
    );
}

/// A Router trap in the post-dispatch callback *before* Confirm leaves the shard's canonical write and
/// pinned `Acquire` durable while the reservation stays `Reserved`. The reclaim driver, reading the
/// `Acquire` present, Confirms it (never cancels) — the reservation is re-confirmable, never lost.
#[test]
fn confirm_then_trap_reservation_is_reconfirmed_by_recovery() {
    let env = install_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    arm_router_fault(&env, FAULT_TRAP_BEFORE_CONFIRM);
    gql_execute_idempotent_as_admin_expect_trap(
        &env,
        &insert_account("c@example.com"),
        "confirm-trap",
    );
    arm_router_fault(&env, FAULT_NONE);

    // Recovery reads the proof, sees the `Acquire` present, and Confirms the held reservation.
    run_router_recovery_after_reservation_ttl(&env);

    // The reservation is now Committed: a same-value duplicate is refused non-retryably.
    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_account("c@example.com"),
        "confirm-trap-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "after recovery re-Confirms, the committed value is non-retryable, got {dup:?}"
    );
}

/// A graph-side trap on the `Acquire` ack — injected *after* the Router's Confirm has durably moved
/// the reservation `Reserved → Committed` and stamped `pending_acquire_ack` — leaves the effect
/// pinned (the mutation still succeeds, Confirm being best-effort). Slice-6 recovery re-acks the
/// still-pinned `Acquire` and clears the pending marker, unpinning it. This is the Confirm→ack
/// boundary the Router-side `confirm_then_trap` cannot reach (it traps *before* Confirm), so together
/// they cover both sides of the commit/ack edge.
///
/// The unpinned outbox alone does not prove the *Router-side* `pending_acquire_ack` was cleared, so
/// the test finishes with a `DELETE` → same-value `INSERT` round-trip: a lingering pending ack would
/// hold the `DELETE`'s `Release` (Release-before-Acquire) and refuse the reuse forever, so a
/// successful reuse is positive proof the marker was cleared.
#[test]
fn confirm_ack_failure_is_reacked_by_recovery() {
    let env = install_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    // Make the owning shard trap when the Router acks the `Acquire`. Confirm still commits (Reserved →
    // Committed + pending_acquire_ack persisted) but the ack call rejects, so the effect stays
    // pinned; the mutation itself succeeds because Confirm is best-effort.
    arm_graph_unique_ack_fault_all_shards(&env, GRAPH_FAULT_TRAP_ON_UNIQUE_ACK);
    gql_execute_idempotent_as_admin(&env, &insert_account("ack@example.com"), "ack-fault");
    assert_eq!(
        graph_unique_outbox_len_all_shards(&env),
        1,
        "the Acquire is still pinned because the Router's ack trapped after Confirm"
    );

    // Clear the fault in its own committed message, then drive recovery: the reclaim driver scans the
    // `Committed`-with-`pending_acquire_ack` reservation, re-reads the proof, re-acks the effect, and
    // clears the pending marker — unpinning the outbox effect.
    arm_graph_unique_ack_fault_all_shards(&env, GRAPH_FAULT_NONE);
    run_router_recovery_after_reservation_ttl(&env);
    assert_eq!(
        graph_unique_outbox_len_all_shards(&env),
        0,
        "recovery re-acked the pinned Acquire and cleared pending_acquire_ack"
    );

    // The value stayed committed throughout: a same-value duplicate is refused non-retryably.
    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_account("ack@example.com"),
        "ack-fault-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "after the re-ack the value is still committed, got {dup:?}"
    );
    // Positive proof that `pending_acquire_ack` was actually cleared (not just the outbox unpinned):
    // delete the vertex and reuse the value. A still-set pending ack would hold the `DELETE`'s
    // `Release` (Release-before-Acquire) and the reuse would be refused; success means the marker is
    // gone and the `Release` freed the value.
    gql_execute_idempotent_as_admin(
        &env,
        &format!("MATCH (n:{LABEL}) DETACH DELETE n"),
        "ack-fault-del",
    );
    gql_execute_idempotent_as_admin(&env, &insert_account("ack@example.com"), "ack-fault-reuse");
    // The successful same-value insert proves the Release was applied and
    // `pending_acquire_ack` no longer held the reservation.
}

/// A pinned `Acquire` outbox effect has its **own** pin-until-acked retention, decoupled from the
/// 9-day graph mutation-journal retention (ADR 0027). With the ack fault armed the `Acquire` stays
/// pinned across the whole window (every recovery re-ack traps), so after the journal is actually
/// evicted past 9 days the pinned proof — and the GC-pinned router record — both survive; once the
/// fault clears, recovery re-acks and the value is durably committed.
#[test]
fn outbox_pinned_reservation_survives_journal_eviction_and_is_confirmed() {
    let env = install_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    // Keep the `Acquire` pinned no matter how often recovery runs: every ack traps until cleared.
    arm_graph_unique_ack_fault_all_shards(&env, GRAPH_FAULT_TRAP_ON_UNIQUE_ACK);
    gql_execute_idempotent_as_admin(&env, &insert_account("o@example.com"), "outbox");

    // Baselines (shard reachable): the graph journaled the completed write, and the `Acquire` is
    // pinned in the outbox.
    assert!(
        graph_mutation_journal_len_all_shards(&env) >= 1,
        "the committed write was recorded in the graph mutation journal"
    );
    assert_eq!(
        graph_unique_outbox_len_all_shards(&env),
        1,
        "the Acquire is pinned in the outbox"
    );

    // Elapse past **both** the 7-day router key TTL (ADR 0025) and the 9-day graph journal retention
    // (ADR 0027), then actually evict the graph mutation journal: every entry is now older than the
    // 9-day window, so the journal drains to empty.
    advance_past_journal_eviction(&env);
    assert_eq!(
        evict_graph_mutation_journal_all_shards(&env),
        0,
        "the 9-day-old graph mutation journal entry is evicted past the ADR 0027 window"
    );

    // The decoupling proof: the pinned `Acquire` survives graph-journal eviction (pin-until-acked
    // retention is independent of the journal), and the held reservation GC-pins the router record so
    // a full mutation-key sweep past the 7-day TTL evicts nothing.
    assert_eq!(
        graph_unique_outbox_len_all_shards(&env),
        1,
        "the pinned Acquire survives graph-journal eviction (decoupled retention)"
    );
    let removed = admin_sweep_mutation_keys(&env, 100_000);
    assert_eq!(
        removed, 0,
        "a reservation-pinned record must not be evicted past the eviction window"
    );

    // Clear the fault: recovery now re-acks the still-pinned `Acquire` (read via the surviving
    // outbox, not the evicted journal) and the value is durably committed — never wrongly cancelled.
    arm_graph_unique_ack_fault_all_shards(&env, GRAPH_FAULT_NONE);
    run_router_recovery_after_reservation_ttl(&env);
    assert_eq!(
        graph_unique_outbox_len_all_shards(&env),
        0,
        "after the fault clears, recovery re-acks and unpins the surviving Acquire"
    );

    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_account("o@example.com"),
        "outbox-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "the value was Confirmed via the pinned outbox, never cancelled, got {dup:?}"
    );
    // The duplicate rejection proves the value was never freed by a wrongful cancel.
}

/// A same-`ClaimId` retry arriving while its reservation is `Reclaiming` is fenced (cannot dispatch a
/// fresh canonical write), so the "recovery-cancels-while-retry-commits" interleaving cannot occur.
/// Once the proof scope is reachable and the `Acquire` is proven absent, recovery cancels the
/// abandoned reservation and the value is reusable.
#[test]
fn reclaiming_fences_same_claim_retry() {
    let env = install_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    // Held reservation: stop both shards so the no-`await` Try persists `Reserved` but the canonical
    // dispatch (to whichever shard owns the value) fails — no `Acquire` is ever written.
    stop_graph_shards_all(&env);
    let _ = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_account("r@example.com"),
        "reclaim-retry",
    );

    // Drive the held reservation into `Reclaiming` (a reclaim proof is now logically in flight).
    let moved = test_force_reclaiming(&env, GRAPH_NAME, LABEL, PROPERTY, "r@example.com");
    assert!(moved, "the held Reserved reservation moved to Reclaiming");

    // The same-key (same `ClaimId`) retry is fenced by `Reclaiming` — it returns in-flight without
    // dispatching a fresh canonical write.
    let fenced = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_account("r@example.com"),
        "reclaim-retry",
    );
    assert!(
        matches!(fenced, RouterError::UniquenessReservationInFlight(_)),
        "a same-ClaimId retry during Reclaiming is fenced as in-flight, got {fenced:?}"
    );

    // Convergence: shards reachable, `Acquire` proven absent ⇒ recovery cancels the abandoned
    // reservation, freeing the value for a fresh key.
    start_graph_shards_all(&env);
    run_router_recovery_after_reservation_ttl(&env);

    gql_execute_idempotent_as_admin(
        &env,
        &insert_account("r@example.com"),
        "reclaim-retry-fresh",
    );
}

/// A `Release` (constrained `DELETE`) observed while its value's `Acquire` is still unconfirmed
/// (owner undetermined) is **held**, not acked: the value is not freed until recovery reconciles the
/// `Acquire` first, after which the held `Release` applies and the value becomes reusable.
#[test]
fn release_before_acquire_is_held_until_acquire_reconciled() {
    let env = install_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    // Commit the canonical write but trap before Confirm: the reservation is `Reserved` with no
    // stamped owner, while the vertex + pinned `Acquire` are durable on the shard.
    arm_router_fault(&env, FAULT_TRAP_BEFORE_CONFIRM);
    gql_execute_idempotent_as_admin_expect_trap(&env, &insert_account("b@example.com"), "rba-ins");
    arm_router_fault(&env, FAULT_NONE);
    // The canonical write is durable, but its label/property projection may still lag the trapped
    // mutation. Drain it before the DELETE so the constrained vertex is actually selected.
    drain_maintenance_via_timer(&env, env.graph_source);
    drain_maintenance_via_timer(&env, env.graph_dest);
    // Delete the constrained vertex: the shard removes it and emits a `Release`, but the value is
    // still `Reserved` (owner undetermined), so the Release is held — not acked.
    gql_execute_idempotent_as_admin(
        &env,
        &format!("MATCH (n:{LABEL}) WHERE n.{PROPERTY} = 'b@example.com' DETACH DELETE n"),
        "rba-del",
    );
    // The held Release did not free the value: a fresh same-value insert is still refused, because
    // the underlying `Acquire` has not been reconciled (Release-before-Acquire).
    let blocked = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_account("b@example.com"),
        "rba-blocked",
    );
    assert!(
        matches!(
            blocked,
            RouterError::UniquenessReservationInFlight(_) | RouterError::UniquenessViolation(_)
        ),
        "the held Release must not free the value before the Acquire is reconciled, got {blocked:?}"
    );
    // Recovery reconciles the `Acquire` first (Commit + stamp owner + ack), after which the held
    // `Release` applies and frees the value.
    run_router_recovery_after_reservation_ttl(&env);

    gql_execute_idempotent_as_admin(&env, &insert_account("b@example.com"), "rba-reuse");
}

/// Two ingress messages racing the same constrained value: exactly one commits, the loser is
/// rejected (retryably in-flight, or non-retryably if the winner already committed).
#[test]
fn concurrent_same_value_one_wins_loser_rejected() {
    let env = install_federation();
    test_declare_unique_constraint(&env, GRAPH_NAME, CONSTRAINT, LABEL, PROPERTY);

    let insert = insert_account("race@example.com");
    let (result_a, result_b) =
        gql_execute_idempotent_pair_concurrent_as_admin(&env, &insert, "race-a", &insert, "race-b");

    let winners = [&result_a, &result_b].iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        winners, 1,
        "exactly one concurrent same-value insert wins; got a={result_a:?} b={result_b:?}"
    );

    let loser = match (&result_a, &result_b) {
        (Err(err), _) | (_, Err(err)) => err,
        _ => unreachable!("exactly one winner asserted above"),
    };
    assert!(
        matches!(
            loser,
            RouterError::UniquenessViolation(_) | RouterError::UniquenessReservationInFlight(_)
        ),
        "the loser is refused as a committed duplicate or an in-flight reservation, got {loser:?}"
    );
}
