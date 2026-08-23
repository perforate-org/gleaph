//! PocketIC: ADR 0074 slice 2a — GRANT/REVOKE grammar end to end.
//!
//! One suite covers the full grant lifecycle on the Router control path: an owner grants
//! `TRAVERSE OUTGOING` on a directed edge label, lists the row back through the owner-only
//! introspection query, and revokes it (exact-key evidence via the missing-row error);
//! every invalid variant (directional modifier on an undirected label, unknown label,
//! unknown property, non-owner caller, anonymous subject, mixed statement block) fails
//! closed with its distinct error before any stable write; stored rows survive a canister
//! upgrade together with the principal-record collection (MemoryId 0 vs MemoryId 55 stay
//! independently readable across a reopen); and open-schema graphs accept only the
//! unoriented traversal form.

use std::collections::BTreeSet;

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, ensure_edge_label, gql_mutate_as_admin,
    install_single_shard_federation, install_single_shard_federation_with_graph_admins, wasm_bytes,
};
use gleaph_router::types::{
    GrantDirectionView, GrantOperationView, GrantResourceKindView, GrantSubjectView,
    GraphGrantSummary,
};

/// The bootstrap admin doubles as the registry owner of [`GRAPH_NAME`] in this fixture.
fn owner(env: &FederationEnv) -> Principal {
    env.admin
}

/// A second, unrelated principal: no caps row, tenant of no graph.
fn stranger() -> Principal {
    Principal::from_slice(&[0xC7; 29])
}

/// A graph admin (tenant) that is not the registry owner.
fn tenant_admin() -> Principal {
    Principal::from_slice(&[0xC9; 29])
}

/// Directed + undirected edge labels bound onto the fixture graph, so directional grant
/// validation has a schema to answer against.
fn bind_typed_schema(env: &FederationEnv) {
    let ddl = format!(
        "CREATE GRAPH TYPE gt {{ NODE Person LABELS Person AS person, \
         DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (person -> person), \
         UNDIRECTED EDGE LIKES LABEL LIKES CONNECTING (person ~ person) }} \
         NEXT CREATE GRAPH {GRAPH_NAME} TYPED gt"
    );
    gql_mutate_as_admin(env, &ddl, "adr0074_bind_typed_schema");
}

/// Raw `gql_mutate` returning the typed Router error so fail-closed variants are
/// observable without panicking.
fn mutate_err(env: &FederationEnv, caller: Principal, query: &str) -> RouterError {
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "gql_mutate",
            Encode!(&query.to_string(), &Vec::<u8>::new(), &"adr0074-grant-key")
                .expect("encode gql_mutate"),
        )
        .expect("gql_mutate call");
    match Decode!(&bytes, Result<u64, RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(_)) => panic!("gql_mutate unexpectedly succeeded for {caller}: {query}"),
        Err(err) => panic!("decode gql_mutate: {err}"),
    }
}

/// Expected error shape of one fail-closed variant.
#[derive(Clone, Copy, Debug)]
enum ExpectedErr {
    InvalidArgument,
    NotFound,
    Forbidden,
}

/// Owner-only grant introspection (`list_graph_grants`) with the typed error surfaced.
fn list_grants(
    env: &FederationEnv,
    caller: Principal,
) -> Result<Vec<GraphGrantSummary>, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "list_graph_grants",
            Encode!(&GRAPH_NAME.to_string()).expect("encode list_graph_grants"),
        )
        .expect("list_graph_grants call");
    Decode!(&bytes, Result<Vec<GraphGrantSummary>, RouterError>).expect("decode list_graph_grants")
}

fn expect_grant_summary(row: &GraphGrantSummary) {
    assert_eq!(row.subject, GrantSubjectView::Public);
    assert_eq!(row.operation, GrantOperationView::Traverse);
    assert_eq!(row.direction, Some(GrantDirectionView::Outgoing));
    assert_eq!(row.resource.kind, GrantResourceKindView::Edge);
    assert_eq!(row.resource.label, "KNOWS");
    assert_eq!(row.resource.property, None);
    assert_eq!(row.expires_at_ns, None);
}

#[test]
fn owner_grants_lists_and_revokes_traverse_outgoing() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    let grantee = stranger();
    let grant = format!(
        "GRANT TRAVERSE OUTGOING ON GRAPH {GRAPH_NAME} EDGES KNOWS TO PRINCIPAL '{}'",
        grantee.to_text()
    );
    let revoke = format!(
        "REVOKE TRAVERSE OUTGOING ON GRAPH {GRAPH_NAME} EDGES KNOWS FROM PRINCIPAL '{}'",
        grantee.to_text()
    );

    // Grant: zero-row acknowledgment, then the row is listed back exactly.
    gql_mutate_as_admin(&env, &grant, "adr0074_grant_traverse_outgoing");
    let listed = list_grants(&env, owner(&env)).expect("owner lists grants");
    assert_eq!(listed.len(), 1, "exactly one grant row expected");
    assert_eq!(
        listed[0].subject,
        GrantSubjectView::Principal(grantee.to_text())
    );
    expect_grant_summary(&listed[0]);

    // A non-tenant cannot observe grant state (ADR 0028 non-disclosure shape).
    let err = list_grants(&env, grantee).expect_err("stranger must not list grants");
    assert!(matches!(err, RouterError::NotFound(_)), "got {err:?}");

    // Revoke removes exactly that row...
    gql_mutate_as_admin(&env, &revoke, "adr0074_revoke_traverse_outgoing");
    let listed = list_grants(&env, owner(&env)).expect("owner lists grants after revoke");
    assert!(
        listed.is_empty(),
        "revoke must remove the row, got {listed:?}"
    );

    // ...and revoking again addresses the now-absent exact key.
    let err = mutate_err(&env, owner(&env), &revoke);
    let RouterError::NotFound(message) = err else {
        panic!("expected NotFound on double revoke, got {err:?}")
    };
    assert!(
        message.contains("no stored grant row"),
        "exact-key miss message expected, got {message}"
    );
}

#[test]
fn invalid_variants_fail_closed_before_mutation() {
    let mut admins = BTreeSet::new();
    admins.insert(tenant_admin());
    let env = install_single_shard_federation_with_graph_admins(admins);
    bind_typed_schema(&env);

    // One pre-existing valid grant: every rejected variant below must leave it untouched.
    let survivor = format!("GRANT TRAVERSE OUTGOING ON GRAPH {GRAPH_NAME} EDGES KNOWS TO PUBLIC");
    gql_mutate_as_admin(&env, &survivor, "adr0074_survivor_grant");
    assert_eq!(
        list_grants(&env, owner(&env)).expect("baseline list").len(),
        1
    );

    let variants: [(Principal, String, &str, ExpectedErr); 6] = [
        // Directional modifier on a declared UNDIRECTED edge label.
        (
            owner(&env),
            format!("GRANT TRAVERSE INCOMING ON GRAPH {GRAPH_NAME} EDGES LIKES TO PUBLIC"),
            "UNDIRECTED",
            ExpectedErr::InvalidArgument,
        ),
        // Unknown edge label in the target graph's catalog.
        (
            owner(&env),
            format!("GRANT MATCH ON GRAPH {GRAPH_NAME} EDGES NOSUCH TO PUBLIC"),
            "NOSUCH",
            ExpectedErr::NotFound,
        ),
        // Unknown property on a known vertex label.
        (
            owner(&env),
            format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Person {{ nosuchprop }} TO PUBLIC"),
            "nosuchprop",
            ExpectedErr::NotFound,
        ),
        // Tenant (graph admin) but not the registry owner: authority violation.
        (
            tenant_admin(),
            format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Person TO PUBLIC"),
            "",
            ExpectedErr::Forbidden,
        ),
        // Anonymous subject: invariant 2 rejects it before any mutation.
        (
            owner(&env),
            format!(
                "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Person TO PRINCIPAL '{}'",
                Principal::anonymous().to_text()
            ),
            "anonymous principal",
            ExpectedErr::InvalidArgument,
        ),
        // Mixed content: authorization statements never combine with other statements.
        (
            owner(&env),
            format!(
                "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Person TO PUBLIC \
                 NEXT MATCH (n) RETURN n"
            ),
            "standalone",
            ExpectedErr::InvalidArgument,
        ),
    ];

    for (idx, (caller, stmt, needle, expected)) in variants.iter().enumerate() {
        let err = mutate_err(&env, *caller, stmt);
        match (expected, err) {
            (ExpectedErr::Forbidden, RouterError::Forbidden) => {}
            (ExpectedErr::NotFound, RouterError::NotFound(message)) => assert!(
                message.contains(needle),
                "variant {idx}: expected payload to mention '{needle}', got '{message}'"
            ),
            (ExpectedErr::InvalidArgument, RouterError::InvalidArgument(message)) => {
                let lowered = message.to_lowercase();
                assert!(
                    lowered.contains(&needle.to_lowercase()),
                    "variant {idx}: expected payload to mention '{needle}', got '{message}'"
                );
            }
            (_, unexpected) => panic!(
                "variant {idx}: wrong error shape, expected {expected:?}, got {unexpected:?}"
            ),
        }
        // Fail-closed evidence: the pre-existing grant row survives unchanged.
        let listed = list_grants(&env, owner(&env)).expect("post-failure list");
        assert_eq!(
            listed.len(),
            1,
            "variant {idx} must not mutate stored grants, got {listed:?}"
        );
    }

    // A complete stranger is denied with the ADR 0028 indistinguishable NotFound.
    let err = mutate_err(
        &env,
        stranger(),
        &format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Person TO PUBLIC"),
    );
    let RouterError::NotFound(graph_name) = err else {
        panic!("expected NotFound for stranger, got {err:?}")
    };
    assert!(graph_name.contains(GRAPH_NAME), "got {graph_name}");
}

#[test]
fn grant_and_caps_rows_survive_canister_upgrade() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    let grantee = stranger();
    gql_mutate_as_admin(
        &env,
        &format!(
            "GRANT TRAVERSE OUTGOING ON GRAPH {GRAPH_NAME} EDGES KNOWS \
             TO PRINCIPAL '{}'",
            grantee.to_text()
        ),
        "adr0074_pre_upgrade_grant",
    );
    let before = list_grants(&env, owner(&env)).expect("pre-upgrade list");
    assert_eq!(before.len(), 1);
    expect_grant_summary(&before[0]);

    // Upgrade router, index, and graph shard in place (same wasm).
    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.router, wasm_bytes("ROUTER_WASM"), empty.clone(), None)
        .expect("upgrade router");
    env.pic
        .upgrade_canister(env.index, wasm_bytes("INDEX_WASM"), empty.clone(), None)
        .expect("upgrade index");
    env.pic
        .upgrade_canister(env.graph_source, wasm_bytes("GRAPH_WASM"), empty, None)
        .expect("upgrade graph");

    // Both stable auth collections reopen independently: principal records (MemoryId 0)
    // and grant rows (MemoryId 55) survive side by side — a shared-MemoryId regression
    // would corrupt one of them loudly here.
    let caps_bytes = env
        .pic
        .query_call(
            env.router,
            owner(&env),
            "my_caps",
            Encode!().expect("encode my_caps"),
        )
        .expect("my_caps call");
    let caps: Vec<String> = Decode!(&caps_bytes, Result<Vec<String>, RouterError>)
        .expect("decode my_caps")
        .expect("caps result");
    assert!(
        !caps.is_empty(),
        "principal record must survive the router upgrade"
    );

    let after = list_grants(&env, owner(&env)).expect("post-upgrade list");
    assert_eq!(before, after, "grant rows must survive the router upgrade");
}

#[test]
fn open_schema_graph_accepts_only_unoriented_traversal() {
    let env = install_single_shard_federation();
    // No typed binding: interning the labels leaves their directedness undeclared.
    ensure_edge_label(&env, "KNOWS");

    // Unoriented traversal is grantable...
    gql_mutate_as_admin(
        &env,
        &format!("GRANT TRAVERSE ON GRAPH {GRAPH_NAME} EDGES KNOWS TO PUBLIC"),
        "adr0074_open_schema_unoriented",
    );
    let listed = list_grants(&env, owner(&env)).expect("open-schema list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].operation, GrantOperationView::Traverse);
    assert_eq!(listed[0].direction, None);

    // ...while directional modifiers fail closed: nothing declares KNOWS directed.
    let err = mutate_err(
        &env,
        owner(&env),
        &format!("GRANT TRAVERSE OUTGOING ON GRAPH {GRAPH_NAME} EDGES KNOWS TO PUBLIC"),
    );
    let RouterError::InvalidArgument(message) = err else {
        panic!("expected InvalidArgument for undeclared directedness, got {err:?}")
    };
    assert!(
        message.contains("directedness"),
        "expected the undeclared-directedness rejection, got {message}"
    );
    // The failed variant left the stored row set untouched.
    assert_eq!(
        list_grants(&env, owner(&env))
            .expect("post-failure list")
            .len(),
        1
    );
}
