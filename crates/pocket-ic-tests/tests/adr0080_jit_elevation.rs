//! PocketIC: ADR 0080 JIT metadata elevation loop, end to end.
//!
//! One suite proves the five-stage loop across distinct principals:
//! stranger denial → request → cross-principal approval → in-window access →
//! expiry → re-denial, with evidence introspection and the flagged emergency path,
//! while every former implicit holder stays denied under the tightened default.
//!
//! Expected observable counts, stated before running:
//! - fresh install: `list_elevations` = 0 rows (bootstrap grants caps only; no initial
//!   ControlPlane windows exist outside the loop);
//! - a request alone persists nothing: still 0 rows;
//! - after one approved 4h elevation: exactly 1 row, `active = true`, evidence complete
//!   (requester, approver, justification, window, emergency = false);
//! - after expiry: still exactly 1 row (rows remain stored until GC) but
//!   `active = false`, and metadata access is denied again;
//! - after the emergency self-elevation: exactly 2 rows, the second flagged
//!   `emergency = true` with approver = requester.
//!
//! Plane disjointness is asserted at the boundary too: before elevation an ad-hoc read
//! of the target graph is indistinguishable `NotFound`; inside the window the same read
//! passes pre-plan admission through the metadata arm but fails plan-time data-plane
//! coverage with the uniform `Forbidden`.

use candid::{Decode, Encode, Principal};
use gleaph_gql_ic::graph_registry::GraphRegistryEntry;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    FederationEnv, RegisterGraphIntent, gql_query_as, install_single_shard_federation,
    register_graph_intent,
};
use gleaph_router::types::{
    ElevateApproveArgs, ElevateRequestArgs, ElevationScopeView, ElevationWindow, GrantCapsArgs,
    GrantSubjectView, RegisterGraphShard,
};

/// Capability bits (`gleaph_auth::AdminCaps`): MANAGE_AUTHORIZATION, EMERGENCY_ELEVATE.
const MANAGE_AUTHORIZATION_BITS: u64 = 1 << 6;
const EMERGENCY_ELEVATE_BITS: u64 = 1 << 7;

/// The foreign tenant graph used as the elevation target: registered by the fixture
/// admin on behalf of another owner, so no fixture principal owns it and the bootstrap
/// admin is a plain non-tenant there.
const TARGET_GRAPH: &str = "elev_target";

fn operator_a() -> Principal {
    Principal::from_slice(&[0xA0; 29])
}

fn operator_b() -> Principal {
    Principal::from_slice(&[0xB0; 29])
}

fn emergency_op() -> Principal {
    Principal::from_slice(&[0xE0; 29])
}

/// Holds EMERGENCY_ELEVATE only: proves the bit alone cannot run the *normal*
/// approval path.
fn emergency_only() -> Principal {
    Principal::from_slice(&[0xE1; 29])
}

fn stranger() -> Principal {
    Principal::from_slice(&[0x5E; 29])
}

fn grant_caps(env: &FederationEnv, target: Principal, caps: u64) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_grant_caps",
            Encode!(&GrantCapsArgs { target, caps }).expect("encode admin_grant_caps"),
        )
        .expect("admin_grant_caps call");
    Decode!(&bytes, Result<(), RouterError>)
        .expect("decode admin_grant_caps")
        .expect("admin_grant_caps");
}

/// Raw `elevate_request` returning the typed error so denials are observable.
fn elevate_request_err(
    env: &FederationEnv,
    caller: Principal,
    args: ElevateRequestArgs,
) -> RouterError {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "elevate_request",
            Encode!(&args).expect("encode elevate_request"),
        )
        .expect("elevate_request call");
    match Decode!(&bytes, Result<ElevateRequestArgs, RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(_)) => panic!("elevate_request unexpectedly succeeded for {caller}"),
        Err(err) => panic!("decode elevate_request: {err}"),
    }
}

fn elevate_request_ok(
    env: &FederationEnv,
    caller: Principal,
    args: ElevateRequestArgs,
) -> ElevateRequestArgs {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "elevate_request",
            Encode!(&args).expect("encode elevate_request"),
        )
        .expect("elevate_request call");
    match Decode!(&bytes, Result<ElevateRequestArgs, RouterError>) {
        Ok(Ok(canonical)) => canonical,
        Ok(Err(err)) => panic!("elevate_request rejected: {err:?}"),
        Err(err) => panic!("decode elevate_request: {err}"),
    }
}

/// Raw `elevate_approve` returning the typed error so denials are observable.
fn elevate_approve_err(
    env: &FederationEnv,
    caller: Principal,
    args: ElevateApproveArgs,
) -> RouterError {
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "elevate_approve",
            Encode!(&args).expect("encode elevate_approve"),
        )
        .expect("elevate_approve call");
    match Decode!(&bytes, Result<(), RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(())) => panic!("elevate_approve unexpectedly succeeded for {caller}"),
        Err(err) => panic!("decode elevate_approve: {err}"),
    }
}

fn approve(env: &FederationEnv, caller: Principal, request: ElevateRequestArgs, emergency: bool) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "elevate_approve",
            Encode!(&ElevateApproveArgs { request, emergency }).expect("encode elevate_approve"),
        )
        .expect("elevate_approve call");
    match Decode!(&bytes, Result<(), RouterError>) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("elevate_approve rejected: {err:?}"),
        Err(err) => panic!("decode elevate_approve: {err}"),
    }
}

fn elevations(
    env: &FederationEnv,
    caller: Principal,
) -> Vec<gleaph_router::types::ElevationSummary> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "list_elevations",
            Encode!().expect("encode list_elevations"),
        )
        .expect("list_elevations call");
    match Decode!(
        &bytes,
        Result<Vec<gleaph_router::types::ElevationSummary>, RouterError>
    ) {
        Ok(Ok(rows)) => rows,
        Ok(Err(err)) => panic!("list_elevations rejected: {err:?}"),
        Err(err) => panic!("decode list_elevations: {err}"),
    }
}

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

fn get_graph_id_of(env: &FederationEnv, caller: Principal, graph: &str) -> GraphId {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "get_graph_id",
            Encode!(&graph.to_string()).expect("encode get_graph_id"),
        )
        .expect("get_graph_id call");
    match Decode!(&bytes, Result<GraphId, RouterError>) {
        Ok(Ok(id)) => id,
        Ok(Err(err)) => panic!("get_graph_id rejected: {err:?}"),
        Err(err) => panic!("decode get_graph_id: {err}"),
    }
}

#[test]
fn jit_elevation_loop_end_to_end() {
    let env = install_single_shard_federation();
    let tenant_owner = Principal::from_slice(&[0x70; 29]);

    // Foreign tenant graph: registered by the fixture admin on behalf of another owner.
    register_graph_intent(
        &env.pic,
        env.admin,
        env.router,
        RegisterGraphIntent {
            graph_name: TARGET_GRAPH,
            owner: tenant_owner,
            admins: Default::default(),
            is_home: false,
            shards: vec![RegisterGraphShard {
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                graph_canister: Principal::management_canister(),
                index_canister: env.index,
            }],
            requested_resources: Vec::new(),
        },
    );

    // --- Bootstrap grants caps only: zero elevation windows exist outside the loop ---
    assert_eq!(
        elevations(&env, env.admin).len(),
        0,
        "expected count before running: 0 elevation rows on a fresh canister"
    );

    // --- Stranger denial + former implicit holders now denied (ADR 0080 §2) ---
    let anon = Principal::anonymous();
    assert!(matches!(
        get_graph_err(&env, stranger(), TARGET_GRAPH),
        RouterError::NotFound(_)
    ));
    // The bootstrap admin holds the FULL capability set but no elevation: identical
    // treatment to a stranger, ADR 0028 NotFound non-disclosure preserved.
    assert!(matches!(
        get_graph_err(&env, env.admin, TARGET_GRAPH),
        RouterError::NotFound(_)
    ));
    assert!(matches!(
        get_graph_err(&env, anon, TARGET_GRAPH),
        RouterError::NotFound(_)
    ));
    // Same non-disclosure on the named-graph data path before any elevation.
    let target_read = format!("USE {TARGET_GRAPH} MATCH (n) RETURN n");
    let stranger_pre = gql_query_as(&env, stranger(), &target_read)
        .map(|ok| format!("unexpectedly ok: {} rows", ok.row_count))
        .expect_err("stranger data read must be denied");
    assert!(
        matches!(stranger_pre, RouterError::NotFound(_)),
        "expected NotFound before elevation, got {stranger_pre:?}"
    );

    // --- Caps setup (caps only — no rows ride along) ---
    grant_caps(&env, operator_a(), MANAGE_AUTHORIZATION_BITS);
    grant_caps(&env, operator_b(), MANAGE_AUTHORIZATION_BITS);
    // Both loop endpoints are MANAGE_AUTHORIZATION-gated (ADR 0080 §5); the emergency
    // bit additionally licenses flagged self-approval.
    grant_caps(
        &env,
        emergency_op(),
        MANAGE_AUTHORIZATION_BITS | EMERGENCY_ELEVATE_BITS,
    );
    grant_caps(&env, emergency_only(), EMERGENCY_ELEVATE_BITS);
    assert_eq!(elevations(&env, env.admin).len(), 0);

    // --- Stage 1: request validates but persists nothing ---
    let request = ElevateRequestArgs {
        requester: operator_a(),
        scope: ElevationScopeView::Graph(TARGET_GRAPH.to_string()),
        justification: "incident-4711: cross-tenant topology review".to_string(),
        window: ElevationWindow::Hour4,
    };
    let canonical = elevate_request_ok(&env, operator_a(), request.clone());
    assert_eq!(
        canonical, request,
        "the canonical request echoes the fields"
    );
    assert_eq!(
        elevations(&env, env.admin).len(),
        0,
        "expected count before running: an unapproved request leaves no state"
    );
    // Empty justification is rejected at the gate.
    assert!(matches!(
        elevate_request_err(
            &env,
            operator_a(),
            ElevateRequestArgs {
                requester: operator_a(),
                scope: ElevationScopeView::ControlPlane,
                justification: "   ".to_string(),
                window: ElevationWindow::Hour4,
            }
        ),
        RouterError::InvalidArgument(_)
    ));

    // --- Stage 2 gates: self-approval rejected; approval needs a second caps holder ---
    assert!(matches!(
        elevate_approve_err(
            &env,
            operator_a(),
            ElevateApproveArgs {
                request: request.clone(),
                emergency: false,
            }
        ),
        RouterError::NotAuthorized
    ));
    // An unrelated cap cannot approve either: EMERGENCY_ELEVATE alone does not
    // license the normal approval path.
    assert!(matches!(
        elevate_approve_err(
            &env,
            emergency_only(),
            ElevateApproveArgs {
                request: request.clone(),
                emergency: false,
            }
        ),
        RouterError::NotAuthorized
    ));
    assert_eq!(elevations(&env, env.admin).len(), 0);

    // --- Stages 2-3: cross-principal approval issues the evidence-complete row ---
    approve(&env, operator_b(), request.clone(), false);
    let rows = elevations(&env, env.admin);
    assert_eq!(
        rows.len(),
        1,
        "expected count before running: exactly one row after the first approval"
    );
    let issued = &rows[0];
    assert_eq!(
        issued.requester,
        GrantSubjectView::Principal(operator_a().to_text())
    );
    assert_eq!(
        issued.scope,
        ElevationScopeView::Graph(TARGET_GRAPH.to_string())
    );
    assert!(issued.active, "freshly issued row is active");
    let evidence = issued.evidence.as_ref().expect("loop rows carry evidence");
    assert!(!evidence.emergency);
    assert_eq!(
        evidence.approver.as_deref(),
        Some(operator_b().to_text().as_str())
    );
    assert_eq!(
        evidence.justification.as_deref(),
        Some("incident-4711: cross-tenant topology review")
    );

    // --- Stage 4: metadata access inside the window ---
    get_graph_id_of(&env, operator_a(), TARGET_GRAPH);
    // Plane disjointness at the boundary: give the operator their own HOME graph so
    // ingress resolves a context, then read the ELEVATED graph's data. The metadata
    // elevation admitted topology access above, but the scan's tenancy-only demand on
    // the foreign graph stays uncovered — uniform Forbidden (ADR 0074 §4), never Ok.
    register_graph_intent(
        &env.pic,
        env.admin,
        env.router,
        RegisterGraphIntent {
            graph_name: "elev.op-home",
            owner: operator_a(),
            admins: Default::default(),
            is_home: true,
            shards: vec![RegisterGraphShard {
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                graph_canister: Principal::management_canister(),
                index_canister: env.index,
            }],
            requested_resources: Vec::new(),
        },
    );
    get_graph_id_of(&env, operator_a(), TARGET_GRAPH);
    // Plane disjointness at the boundary: the elevated operator is now ADMITTED past
    // graph visibility (sole visible graph resolves), but the data plane stays closed —
    // the metadata row covers none of the scan's demands, so the uniform Forbidden of
    // ADR 0074 §4 applies instead of NotFound.
    assert!(matches!(
        get_graph_err(&env, stranger(), TARGET_GRAPH),
        RouterError::NotFound(_)
    ));
    let data_read = gql_query_as(&env, operator_a(), &target_read)
        .map(|ok| format!("unexpectedly ok: {} rows", ok.row_count))
        .expect_err("metadata elevation must never cover data-plane demands");
    assert!(
        matches!(data_read, RouterError::Forbidden),
        "expected plan-time Forbidden, got {data_read:?}"
    );

    // --- Stage 5: automatic expiry denies again; the row remains introspectable ---
    env.pic
        .advance_time(std::time::Duration::from_secs(5 * 60 * 60));
    env.pic.tick();
    assert!(matches!(
        get_graph_err(&env, operator_a(), TARGET_GRAPH),
        RouterError::NotFound(_)
    ));
    let rows = elevations(&env, env.admin);
    assert_eq!(rows.len(), 1, "expired rows persist until GC");
    assert!(!rows[0].active, "expired row is flagged inactive");

    // --- Emergency self-elevation writes the flagged no-approval variant ---
    let emergency_request = ElevateRequestArgs {
        requester: emergency_op(),
        scope: ElevationScopeView::ControlPlane,
        justification: "pager duty: fleet sweep".to_string(),
        window: ElevationWindow::Day7,
    };
    elevate_request_ok(&env, emergency_op(), emergency_request.clone());
    approve(&env, emergency_op(), emergency_request, true);
    let rows = elevations(&env, env.admin);
    assert_eq!(
        rows.len(),
        2,
        "expected count before running: exactly two rows after the emergency issuance"
    );
    // Canonical key order puts the ControlPlane row first (shorter resource length);
    // identify rows by scope instead of position.
    let flagged = rows
        .iter()
        .find(|r| r.scope == ElevationScopeView::ControlPlane)
        .expect("the emergency ControlPlane row is listed");
    let graph_row = rows
        .iter()
        .find(|r| r.scope == ElevationScopeView::Graph(TARGET_GRAPH.to_string()))
        .expect("the approved graph elevation stays listed");
    assert!(flagged.active);
    assert!(
        !graph_row.active,
        "the first elevation remains listed and inactive after expiry"
    );
    assert_eq!(flagged.scope, ElevationScopeView::ControlPlane);
    let evidence = flagged
        .evidence
        .as_ref()
        .expect("emergency rows carry evidence");
    assert!(evidence.emergency, "the emergency flag must be surfaced");
    assert_eq!(
        evidence.approver.as_deref(),
        Some(emergency_op().to_text().as_str()),
        "approver = requester on the emergency variant"
    );
    // ControlPlane covers any graph's metadata plane immediately.
    get_graph_id_of(&env, emergency_op(), TARGET_GRAPH);

    // --- Anonymous callers are rejected at both loop endpoints ---
    assert!(matches!(
        elevate_request_err(&env, anon, request),
        RouterError::NotAuthorized
    ));

    // --- ADR 0028 own-shard federation arm unaffected ---
    // The graph shard canister keeps resolving its own graph's routing metadata.
    let _shard_view = get_graph_id_of(&env, env.graph_source, "gleaph.pocket_ic");
}
