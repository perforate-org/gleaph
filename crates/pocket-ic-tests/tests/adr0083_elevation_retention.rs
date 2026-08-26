//! PocketIC: ADR 0083 — expired elevation-row retention GC, end to end.
//!
//! Timer-driven through the established time-advance/drain harness: each round advances
//! simulated time past whatever delay the autonomous timer last scheduled (mid-lap
//! continuation or idle heartbeat) and ticks until the fired message settles, executing
//! exactly one bounded sweep step per round.
//!
//! Expected observable counts, stated before running (per test):
//!
//! `expired_elevation_rows_drain_across_ticks_after_the_review_window`
//! - fresh install: `list_elevations` = 0 rows;
//! - after seeding two grammar-written standing grants (data-plane `TRAVERSE OUTGOING`,
//!   `READ_METADATA`) plus 20 approved 4h-loop elevations on the fixture graph:
//!   `list_elevations` = 20 active rows, `list_graph_grants` = 3 (implicit-root marker +
//!   the two standing rows);
//! - after 5h (past expiry, inside the constant 90-day review window): still exactly
//!   20 listed rows, all inactive, metadata access re-denied — the review surface
//!   survives full expiry;
//! - after jumping past the review window and ONE timer round: strictly fewer than 20
//!   but at least one elevation remains (bounded per tick: budget 16 visited keys < 22
//!   stored keys, so one step cannot drain the backlog);
//! - after further timer rounds: `list_elevations` = 0, reached only across multiple
//!   rounds (resumable-cursor proof), while `list_graph_grants` stays exactly 3 —
//!   standing non-expiring rows are never swept — and enforcement denies the elevated
//!   operator identically before and after the sweep.
//!
//! `reissued_elevation_supersedes_prior_evidence_and_is_swept_per_window`
//! - after issuing and then re-issuing the same (requester, scope) elevation: exactly
//!   1 listed row carrying the SECOND evidence (`GrantKey` carries no issuance time —
//!   accepted v1 supersession semantics, ADR 0083 invariant 3 trade-off);
//! - the replacement is itself retained through its own review window and swept after
//!   it: eventually 0 rows.
//!
//! `no_public_mutation_path_touches_stored_retention_state`
//! - one issued elevation stays exactly 1 listed row after an adversarial walk of every
//!   public surface a stranger might use to mutate grant rows (`list_elevations`,
//!   `admin_grant_caps`, grammar `GRANT` / `REVOKE`) — all rejected, nothing mutated;
//!   no caller-facing sweep trigger exists at all.
//!
//! Grammar note ([ADR 0080] §5 / [ADR 0083] §2): `GRANT` statements generate only
//! standing (non-expiring) rows — the windowed form flows exclusively through the
//! elevation loop. This suite therefore proves the grammar-written *standing* form is
//! never swept; the rule's genericity over `expires_at` on every expiring row shape
//! (including evidence-free metadata rows) is pinned by the gleaph-auth unit matrix.

use candid::{Decode, Encode, Principal};
use gleaph_gql_ic::graph_registry::GraphRegistryEntry;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, gql_mutate_as_admin, install_single_shard_federation,
};
use gleaph_router::types::{
    ElevateApproveArgs, ElevateRequestArgs, ElevationScopeView, ElevationWindow, GrantCapsArgs,
    GraphGrantSummary,
};

/// Capability bit (`gleaph_auth::AdminCaps::MANAGE_AUTHORIZATION`).
const MANAGE_AUTHORIZATION_BITS: u64 = 1 << 6;

/// Expired-elevation backlog size: larger than one tick's bounded slice (16 visited
/// keys), so a single sweep step provably cannot drain it.
const BACKLOG: usize = 20;

/// The autonomous retention tick always reschedules at least this far into the future
/// (lap continuation is prompt; a completed lap waits out its daily-scale heartbeat), so
/// advancing this much per round crosses exactly the next deadline: one bounded sweep
/// step executes per round.
const RETENTION_ROUND_ADVANCE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

fn operator_a() -> Principal {
    Principal::from_slice(&[0xA0; 29])
}

fn operator_b() -> Principal {
    Principal::from_slice(&[0xB0; 29])
}

fn stranger() -> Principal {
    Principal::from_slice(&[0x5E; 29])
}

fn backlog_requester(index: usize) -> Principal {
    debug_assert!(index < BACKLOG);
    Principal::from_slice(&[0x80 + index as u8; 29])
}

fn grant_caps(env: &FederationEnv, caller: Principal, target: Principal, caps: u64) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "admin_grant_caps",
            Encode!(&GrantCapsArgs { target, caps }).expect("encode admin_grant_caps"),
        )
        .expect("admin_grant_caps call");
    Decode!(&bytes, Result<(), RouterError>)
        .expect("decode admin_grant_caps")
        .expect("admin_grant_caps");
}

/// Raw `admin_grant_caps` returning the typed error so denials stay observable.
fn grant_caps_err(env: &FederationEnv, caller: Principal, target: Principal) -> RouterError {
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "admin_grant_caps",
            Encode!(&GrantCapsArgs {
                target,
                caps: u64::MAX,
            })
            .expect("encode admin_grant_caps"),
        )
        .expect("admin_grant_caps call");
    match Decode!(&bytes, Result<(), RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(())) => panic!("admin_grant_caps unexpectedly succeeded for {caller}"),
        Err(err) => panic!("decode admin_grant_caps: {err}"),
    }
}

/// Issues one loop elevation: `elevate_request` filed by a `MANAGE_AUTHORIZATION`
/// holder on the requester's behalf (validated no-op), then cross-principal approval,
/// which writes the evidence-complete row ([ADR 0080] §3).
fn elevate(
    env: &FederationEnv,
    requester: Principal,
    justification: &str,
    window: ElevationWindow,
) {
    let request = ElevateRequestArgs {
        requester,
        scope: ElevationScopeView::Graph(GRAPH_NAME.to_string()),
        justification: justification.to_string(),
        window,
    };
    let bytes = env
        .pic
        .query_call(
            env.router,
            operator_a(),
            "elevate_request",
            Encode!(&request).expect("encode elevate_request"),
        )
        .expect("elevate_request call");
    Decode!(&bytes, Result<ElevateRequestArgs, RouterError>)
        .expect("decode elevate_request")
        .expect("elevate_request");

    let bytes = env
        .pic
        .update_call(
            env.router,
            operator_b(),
            "elevate_approve",
            Encode!(&ElevateApproveArgs {
                request,
                emergency: false
            })
            .expect("encode elevate_approve"),
        )
        .expect("elevate_approve call");
    Decode!(&bytes, Result<(), RouterError>)
        .expect("decode elevate_approve")
        .expect("elevate_approve");
}

fn elevations(env: &FederationEnv) -> Vec<gleaph_router::types::ElevationSummary> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "list_elevations",
            Encode!().expect("encode list_elevations"),
        )
        .expect("list_elevations call");
    Decode!(
        &bytes,
        Result<Vec<gleaph_router::types::ElevationSummary>, RouterError>
    )
    .expect("decode list_elevations")
    .expect("list_elevations")
}

fn graph_grants(env: &FederationEnv) -> Vec<GraphGrantSummary> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "list_graph_grants",
            Encode!(&GRAPH_NAME.to_string()).expect("encode list_graph_grants"),
        )
        .expect("list_graph_grants call");
    Decode!(&bytes, Result<Vec<GraphGrantSummary>, RouterError>)
        .expect("decode list_graph_grants")
        .expect("list_graph_grants")
}

/// Raw `get_graph` returning the typed error so denial shapes stay observable.
fn get_graph_err(env: &FederationEnv, caller: Principal, graph: &str) -> RouterError {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "get_graph",
            Encode!(&graph.to_string()).expect("encode get_graph"),
        )
        .expect("get_graph call");
    match Decode!(&bytes, Result<GraphRegistryEntry, RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(_)) => panic!("get_graph unexpectedly succeeded for {caller}"),
        Err(err) => panic!("decode get_graph: {err}"),
    }
}

/// Raw `gql_mutate` with an explicit caller, returning the typed Router error.
fn mutate_err(env: &FederationEnv, caller: Principal, query: &str) -> RouterError {
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "gql_mutate",
            Encode!(
                &query.to_string(),
                &Vec::<u8>::new(),
                &"adr0083-adversarial"
            )
            .expect("encode gql_mutate"),
        )
        .expect("gql_mutate call");
    match Decode!(&bytes, Result<u64, RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(_)) => panic!("gql_mutate unexpectedly succeeded for {caller}: {query}"),
        Err(err) => panic!("decode gql_mutate: {err}"),
    }
}

/// One timer-driven sweep round: advance simulated time past whatever delay the
/// retention tick last scheduled, then tick until the fired message settles. Exactly
/// one bounded sweep step executes per round because every tick reschedules at least
/// [`RETENTION_ROUND_ADVANCE`] into the future.
fn sweep_round(env: &FederationEnv) {
    env.pic.advance_time(RETENTION_ROUND_ADVANCE);
    for _ in 0..4 {
        env.pic.tick();
    }
}

/// Drives timer rounds until `list_elevations` drains to zero; returns the number of
/// rounds consumed so callers can prove multi-tick draining.
fn drain_elevations_via_timer(env: &FederationEnv) -> usize {
    const MAX_ROUNDS: usize = 12;
    for round in 0..MAX_ROUNDS {
        if elevations(env).is_empty() {
            return round;
        }
        sweep_round(env);
    }
    assert_eq!(
        elevations(env).len(),
        0,
        "retention timer failed to drain expired elevations within {MAX_ROUNDS} rounds"
    );
    MAX_ROUNDS
}

/// Caps setup plus the two grammar-written standing grants shared by the suites.
fn seed_federation_with_standing_grants(env: &FederationEnv, dp_holder: Principal) {
    // Bind a typed schema declaring KNOWS directed, so the directional traversal grant
    // is expressible (mirrors the adr0074 fixture).
    gql_mutate_as_admin(
        env,
        &format!(
            "CREATE GRAPH TYPE gt {{ NODE Person LABELS Person AS person, \
             DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (person -> person) }} \
             NEXT CREATE GRAPH {GRAPH_NAME} TYPED gt"
        ),
        "adr0083_bind_typed_schema",
    );
    grant_caps(env, env.admin, operator_a(), MANAGE_AUTHORIZATION_BITS);
    grant_caps(env, env.admin, operator_b(), MANAGE_AUTHORIZATION_BITS);

    // Standing data-plane row and standing grammar-form metadata row (ADR 0074 §5 /
    // ADR 0080 §5): `GRANT` never writes an expiry, so these must survive every sweep.
    gql_mutate_as_admin(
        env,
        &format!(
            "GRANT TRAVERSE OUTGOING ON GRAPH {GRAPH_NAME} EDGES KNOWS TO PRINCIPAL '{}'",
            dp_holder.to_text()
        ),
        "adr0083_seed_dp_traverse",
    );
    gql_mutate_as_admin(
        env,
        &format!(
            "GRANT READ_METADATA ON GRAPH {GRAPH_NAME} TO PRINCIPAL '{}'",
            dp_holder.to_text()
        ),
        "adr0083_seed_meta_standing",
    );
    assert_eq!(
        graph_grants(env).len(),
        3,
        "expected count before running: implicit-root marker plus the two standing rows"
    );
}

#[test]
fn expired_elevation_rows_drain_across_ticks_after_the_review_window() {
    let env = install_single_shard_federation();
    let dp_holder = Principal::from_slice(&[0xC1; 29]);

    assert_eq!(
        elevations(&env).len(),
        0,
        "expected count before running: 0 elevation rows on a fresh federation"
    );
    seed_federation_with_standing_grants(&env, dp_holder);

    // Seed the expired backlog through the real elevation loop: 20 distinct requesters,
    // 4h windows, all approved cross-principal.
    for index in 0..BACKLOG {
        elevate(
            &env,
            backlog_requester(index),
            &format!("incident-{index}: retention backlog"),
            ElevationWindow::Hour4,
        );
    }
    let rows = elevations(&env);
    assert_eq!(
        rows.len(),
        BACKLOG,
        "expected count before running: exactly {BACKLOG} seeded elevation rows"
    );
    assert!(rows.iter().all(|row| row.active));

    // Past expiry, inside the constant review window: every row stays listed (inactive),
    // and enforcement already treats them as absent — GC changes no verdict later.
    env.pic
        .advance_time(std::time::Duration::from_secs(5 * 60 * 60));
    env.pic.tick();
    let rows = elevations(&env);
    assert_eq!(
        rows.len(),
        BACKLOG,
        "the review window keeps expired rows introspectable"
    );
    assert!(rows.iter().all(|row| !row.active));
    assert!(matches!(
        get_graph_err(&env, backlog_requester(0), GRAPH_NAME),
        RouterError::NotFound(_)
    ));

    // Jump past the review window (expiry + 90 days): the backlog is now sweepable.
    env.pic
        .advance_time(std::time::Duration::from_secs(91 * 24 * 60 * 60));

    // Bounded-slice proof: one timer round cannot drain a backlog larger than the
    // per-tick budget (16 visited keys < 22 stored keys).
    sweep_round(&env);
    let remaining_after_one_round = elevations(&env).len();
    assert!(
        remaining_after_one_round > 0 && remaining_after_one_round < BACKLOG,
        "one bounded sweep step must leave part of a {BACKLOG}-row backlog, got \
         {remaining_after_one_round} remaining"
    );

    // Resumable-cursor proof: successive timer rounds drain the rest without skips.
    // Together with the partial round above, the backlog provably drained across
    // multiple ticks (one step per round; the first round alone removed at most its
    // bounded slice).
    let rounds_used = drain_elevations_via_timer(&env);
    assert!(
        rounds_used >= 1,
        "finishing the drain must take further timer rounds, took {rounds_used}"
    );
    assert_eq!(elevations(&env).len(), 0);

    // Standing rows are never swept and the read surface is otherwise unchanged.
    let grants = graph_grants(&env);
    assert_eq!(
        grants.len(),
        3,
        "implicit-root marker plus both standing rows must survive every sweep"
    );
    assert!(grants.iter().all(|row| row.expires_at_ns.is_none()));
    assert!(matches!(
        get_graph_err(&env, backlog_requester(0), GRAPH_NAME),
        RouterError::NotFound(_)
    ));
}

#[test]
fn reissued_elevation_supersedes_prior_evidence_and_is_swept_per_window() {
    let env = install_single_shard_federation();
    let dp_holder = Principal::from_slice(&[0xC2; 29]);
    seed_federation_with_standing_grants(&env, dp_holder);

    // First issuance, then a re-issue of the SAME (requester, scope) while the prior
    // row sits inside its review window: the canonical key carries no issuance time,
    // so the replacement destroys the prior evidence (accepted v1 semantics).
    elevate(
        &env,
        operator_a(),
        "first: superseded evidence",
        ElevationWindow::Hour4,
    );
    elevate(
        &env,
        operator_a(),
        "second: surviving evidence",
        ElevationWindow::Hour4,
    );

    let rows = elevations(&env);
    assert_eq!(
        rows.len(),
        1,
        "expected count before running: exactly one row after the superseding re-issue"
    );
    let evidence = rows[0].evidence.as_ref().expect("loop rows carry evidence");
    assert_eq!(
        evidence.justification.as_deref(),
        Some("second: surviving evidence")
    );
    assert_eq!(
        evidence.approver.as_deref(),
        Some(operator_b().to_text().as_str())
    );
    assert!(rows[0].active);

    // The replacement is itself retained per the window, then swept after it.
    env.pic
        .advance_time(std::time::Duration::from_secs(5 * 60 * 60));
    env.pic.tick();
    let rows = elevations(&env);
    assert_eq!(rows.len(), 1, "replacement kept through its review window");
    assert!(!rows[0].active);

    env.pic
        .advance_time(std::time::Duration::from_secs(91 * 24 * 60 * 60));
    let rounds_used = drain_elevations_via_timer(&env);
    assert!(
        rounds_used >= 1,
        "the replacement row must be swept by the autonomous timer, drained in {rounds_used}"
    );
    assert_eq!(elevations(&env).len(), 0);
    assert_eq!(graph_grants(&env).len(), 3, "standing rows untouched");
}

#[test]
fn no_public_mutation_path_touches_stored_retention_state() {
    let env = install_single_shard_federation();
    let dp_holder = Principal::from_slice(&[0xC3; 29]);
    seed_federation_with_standing_grants(&env, dp_holder);
    elevate(
        &env,
        operator_a(),
        "incident-9: adversarial probe",
        ElevationWindow::Hour4,
    );
    assert_eq!(
        elevations(&env).len(),
        1,
        "baseline before the adversarial walk"
    );
    let baseline_grants = graph_grants(&env);

    // The review surface is caps-gated...
    let bytes = env
        .pic
        .query_call(
            env.router,
            stranger(),
            "list_elevations",
            Encode!().expect("encode list_elevations"),
        )
        .expect("list_elevations call");
    assert!(
        matches!(
            Decode!(
                &bytes,
                Result<Vec<gleaph_router::types::ElevationSummary>, RouterError>
            )
            .expect("decode list_elevations"),
            Err(RouterError::NotAuthorized)
        ),
        "a stranger must not reach the review surface"
    );

    // ...and every public mutation candidate is denied for an unauthorized caller:
    // self-granted capabilities, a grammar GRANT, and a grammar REVOKE.
    assert!(
        matches!(
            grant_caps_err(&env, stranger(), stranger()),
            RouterError::NotAuthorized
        ),
        "a stranger must not mint capabilities"
    );
    assert!(matches!(
        mutate_err(
            &env,
            stranger(),
            &format!(
                "GRANT READ_METADATA ON GRAPH {GRAPH_NAME} TO PRINCIPAL '{}'",
                stranger().to_text()
            )
        ),
        RouterError::NotFound(_) | RouterError::Forbidden
    ));
    assert!(matches!(
        mutate_err(
            &env,
            stranger(),
            &format!("REVOKE READ_METADATA ON GRAPH {GRAPH_NAME} FROM PUBLIC")
        ),
        RouterError::NotFound(_) | RouterError::Forbidden
    ));

    // Nothing mutated: the issued row and both standing rows survive untouched, and no
    // caller-facing sweep trigger exists anywhere on the API surface.
    assert_eq!(
        elevations(&env).len(),
        1,
        "denied attempts persist nothing and remove nothing"
    );
    assert_eq!(
        graph_grants(&env),
        baseline_grants,
        "stored grant rows are byte-identical after the walk"
    );
}
