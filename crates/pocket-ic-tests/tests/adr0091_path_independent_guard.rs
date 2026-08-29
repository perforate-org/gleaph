//! PocketIC proof for ADR 0091: the path-independent `CanonicalSegmentGuard`
//! traps on the first inter-canister chokepoint reached from inside a canonical
//! mutation segment, rolling back the whole message (Property 5).
//!
//! Three scenarios exercise the guard:
//!
//! 1. `canonical_segment_with_no_inter_canister_call_succeeds` — a normal
//!    shard-local DML commits cleanly with the guard active. The guard must
//!    not interfere with the happy path; depth returns to zero after the
//!    segment exits.
//!
//! 2. `read_path_outside_canonical_segment_can_call_index` — a read query
//!    that reaches the graph-index chokepoint (`ExecuteCtx::new`) runs with
//!    the guard **not** active. The chokepoint's
//!    `assert_no_canonical_segment("executor_context_new")` must pass.
//!
//! 3. `inter_canister_call_inside_canonical_segment_traps_whole_message` —
//!    a canonical segment whose write tail reaches the E2E seam
//!    `GLEAPH.E2E_SIMULATE_INTER_CANISTER_CALL()` traps the whole message.
//!    Pre-existing canonical and derived state survives unchanged. The trap
//!    surface is `assert_no_canonical_segment` firing from inside the
//!    segment, with the depth non-zero.

use candid::{Decode, Encode};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, ensure_vertex_label, gql_mutate_as_admin, gql_query_as_admin,
    install_single_shard_federation,
};

fn count(env: &FederationEnv, query: &str) -> u64 {
    gql_query_as_admin(env, query).row_count
}

#[test]
fn canonical_segment_with_no_inter_canister_call_succeeds() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "GuardHappy");

    // A bare INSERT runs entirely inside the canonical segment; no
    // inter-canister call is reached; the guard enters and exits cleanly.
    let row_count = gql_mutate_as_admin(&env, "INSERT (:GuardHappy)", "adr0091_happy_insert");
    assert_eq!(row_count, 0, "bare INSERT projects zero rows");

    // The vertex is observable — the segment committed normally.
    assert_eq!(
        count(&env, "MATCH (n:GuardHappy) RETURN n"),
        1,
        "happy-path canonical segment must commit and survive the guard exit"
    );
}

#[test]
fn read_path_outside_canonical_segment_can_call_index() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "GuardReadSeed");
    // Seed a vertex so the read has a meaningful, non-empty result.
    let _ = gql_mutate_as_admin(&env, "INSERT (:GuardReadSeed)", "adr0091_read_seed");

    // A read query reaches `ExecuteCtx::new`, the `PropertyIndexLookup`
    // chokepoint. The chokepoint's `assert_no_canonical_segment` must pass:
    // the read path runs **outside** the canonical segment, so the depth
    // is zero and the assertion holds. A trap here would mean the guard
    // wrongly fires for legitimate read traffic.
    let result = gql_query_as_admin(&env, "MATCH (n:GuardReadSeed) RETURN n");
    assert_eq!(
        result.row_count, 1,
        "read path must not be blocked by the canonical segment guard"
    );
}

/// Issue a router `gql_mutate` that must NOT commit because the graph
/// shard traps inside its DML atomic section. Accepts both `Ok(Err(_))`
/// (a router error carrying the trap message) and `Err(reject)` (a raw
/// call rejection carrying the canister trap message) — the existing
/// rollback test uses this same shape. Asserting the surfaced error
/// mentions `inter-canister call` proves the trap originated from
/// `assert_no_canonical_segment` (ADR 0091), not from an unrelated
/// pre-execution parse/plan rejection.
fn gql_mutate_expect_inter_canister_trap(
    env: &FederationEnv,
    query: &str,
    client_mutation_key: &str,
) {
    let outcome = env.pic.update_call(
        env.router,
        env.admin,
        "gql_mutate",
        Encode!(
            &query.to_string(),
            &Vec::<u8>::new(),
            &client_mutation_key.to_string()
        )
        .expect("encode gql_mutate"),
    );
    let message = match outcome {
        Ok(reply) => match Decode!(&reply, Result<GqlQueryResult, RouterError>) {
            Ok(Err(err)) => format!("{err:?}"),
            Ok(Ok(result)) => panic!(
                "trapping DML must not commit, got row_count {}",
                result.row_count
            ),
            Err(err) => panic!("decode gql_mutate: {err}"),
        },
        Err(reject) => format!("{reject:?}"),
    };
    assert!(
        message.contains("inter-canister call") || message.contains("canonical mutation segment"),
        "expected an inter-canister-call trap from assert_no_canonical_segment, got error: {message}"
    );
}

#[test]
fn inter_canister_call_inside_canonical_segment_traps_whole_message() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "GuardPreExisting");
    ensure_vertex_label(&env, "GuardTrapOrphan");

    // Seed pre-existing canonical state. The rollback assertion checks these
    // vertices (and their count) survive unchanged.
    let _ = gql_mutate_as_admin(
        &env,
        "INSERT (:GuardPreExisting)",
        "adr0091_seed_preexisting",
    );
    assert_eq!(count(&env, "MATCH (n:GuardPreExisting) RETURN n"), 1);

    // The trap plan: a bare INSERT (canonical write) immediately followed by
    // a CALL to the E2E seam procedure. The CALL runs **inside** the same
    // canonical mutation segment, fires
    // `assert_no_canonical_segment("e2e_simulate_inter_canister_call_inside_segment")`,
    // and the depth is non-zero — so the assertion fails, the message traps,
    // and Property 5 rolls back the whole message.
    //
    // The INSERT is what makes the trap detectable as rollback rather than a
    // write that never happened: the canonical write would have committed if
    // the guard were missing or the assertion were a no-op. Removing the
    // `enter()` call from `apply_canonical_mutation_segment` would make this
    // test fail: the depth would be zero, the assertion would pass, the
    // orphan would be written, and the post-trap count would be 1, not 0.
    // Removing the `assert_no_canonical_segment` call from the seam
    // function would make this test fail: the seam would no-op, the
    // orphan would be written, and the post-trap count would be 1, not 0.
    gql_mutate_expect_inter_canister_trap(
        &env,
        "INSERT (:GuardTrapOrphan) NEXT CALL GLEAPH.E2E_SIMULATE_INTER_CANISTER_CALL()",
        "adr0091_inter_canister_trap",
    );

    // Whole-message rollback: the trap fired inside the canonical segment,
    // so the orphan inserted in the same segment must not survive.
    assert_eq!(
        count(&env, "MATCH (n:GuardTrapOrphan) RETURN n"),
        0,
        "whole-message rollback: the orphan written before the trap must not persist"
    );
    // And the pre-existing canonical state must be untouched.
    assert_eq!(
        count(&env, "MATCH (n:GuardPreExisting) RETURN n"),
        1,
        "whole-message rollback must not disturb pre-existing canonical state"
    );
}
