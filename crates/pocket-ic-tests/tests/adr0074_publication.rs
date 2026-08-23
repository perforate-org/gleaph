//! PocketIC: ADR 0074 slice 3 — caller-bounded prepared-query publication end to end.
//!
//! One suite proves the full publication lifecycle over the Router control path:
//!
//! 1. Registration stores static requirements and publishes nothing: anonymous execution
//!    is default-denied until the owner issues a bounded
//!    `GRANT EXECUTE ON PREPARED QUERY <name> TO PUBLIC`; `REVOKE` denies it again.
//!    The listing surfaces the synthesized implicit-root marker (invariant 3) instead of
//!    an apparently empty list.
//! 2. Invariant 7 (PUBLIC never exceeds its publisher): a `PREPARE_REGISTER` granter who
//!    is no tenant cannot publish a scan-shaped query while missing even one requirement;
//!    once every requirement is covered, publication succeeds.
//! 3. Replacing a record cascades its stale publication rows: after an upsert changes the
//!    requirement set, the previously published PUBLIC row is gone and must be explicitly
//!    re-issued against the new set (anonymous denial despite PUBLIC data-plane rows).
//! 4. Publication rows persist across a router upgrade together with the record's static
//!    requirement set.
//!
//! Expected counts stated before running (per plan contract):
//! - 4 `#[test]` functions.
//! - Each test constructs one PocketIC environment (`install_single_shard_federation`):
//!   3 funded `create_canister` calls and exactly 3 canister installs (Router, Index,
//!   Graph shard) per environment.
//! - Update/query call budget per environment: ≤1 schema-bind mutate, ≤3 seed mutates with
//!   3 maintenance drains (lifecycle test only), ≤7 GRANT mutates, ≤4 bounded
//!   publication/revoke mutates, ≤1 `admin_grant_caps`, ≤3 `prepare` batches, ≤1 upgrade
//!   triple, and ≤10 probe query/update calls.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::{GqlQueryResult, ReadMode};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, drain_maintenance_via_timer, gql_mutate_as_admin,
    install_single_shard_federation, prepare_batch_as_admin, prepared_query_with_params_as,
    publish_prepared_query_as, revoke_prepared_publication_as, wasm_bytes,
};
use gleaph_router::types::{
    GrantCapsArgs, GrantOperationView, GrantResourceKindView, GrantSubjectView, GraphGrantSummary,
};

/// A `PREPARE_REGISTER`-only principal: publication authority without tenancy or rows.
fn publisher() -> Principal {
    Principal::from_slice(&[0xE1; 29])
}

/// Bind the typed schema used by the scan-shaped fixtures: Person{name} and a DIRECTED
/// KNOWS edge.
fn bind_typed_schema(env: &FederationEnv) {
    let ddl = format!(
        "CREATE GRAPH TYPE gt {{ NODE Person {{ name STRING }}, \
         DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) }} \
         NEXT CREATE GRAPH {} TYPED gt",
        GRAPH_NAME
    );
    gql_mutate_as_admin(env, &ddl, "publication-bind-schema");
}

/// Seed alice -[KNOWS]-> bob for execution-content assertions.
fn seed_two_persons_and_one_edge(env: &FederationEnv) {
    gql_mutate_as_admin(
        env,
        "INSERT (:Person {name: 'alice'})",
        "publication-seed-alice",
    );
    drain_maintenance_via_timer(env, env.graph_source);
    gql_mutate_as_admin(
        env,
        "INSERT (:Person {name: 'bob'})",
        "publication-seed-bob",
    );
    drain_maintenance_via_timer(env, env.graph_source);
    gql_mutate_as_admin(
        env,
        "MATCH (a:Person {name: 'alice'}) RETURN a NEXT \
         MATCH (b:Person {name: 'bob'}) \
         INSERT (a)-[:KNOWS]->(b)",
        "publication-seed-knows",
    );
    drain_maintenance_via_timer(env, env.graph_source);
}

/// The PUBLIC data-plane rows the fixture queries' static sets demand.
fn grant_public_data_rows(env: &FederationEnv) {
    for statement in [
        format!("GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Person TO PUBLIC"),
        format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Person TO PUBLIC"),
        format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Person {{ name }} TO PUBLIC"),
        format!("GRANT TRAVERSE ON GRAPH {GRAPH_NAME} EDGES KNOWS TO PUBLIC"),
    ] {
        gql_mutate_as_admin(env, &statement, "publication-grant-public-data");
    }
}

/// Grants the MATCH and READ rows to an explicit principal, leaving traversal ungranted
/// (the invariant-7 "exactly one requirement missing" fixture shape).
fn grant_read_rows_to(env: &FederationEnv, subject: &str) {
    for statement in [
        format!("GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Person TO {subject}"),
        format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Person TO {subject}"),
    ] {
        gql_mutate_as_admin(env, &statement, "publication-grant-granter-rows");
    }
}

fn prepare_registration(name: &str, query: &str) -> gleaph_prepared_api::PreparedRegistration {
    gleaph_prepared_api::PreparedRegistration {
        name: name.into(),
        query: query.into(),
        metadata: None,
    }
}

/// Raw `prepared_query` returning the typed error so denials are observable.
fn prepared_query_err(env: &FederationEnv, caller: Principal, name: &str) -> RouterError {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "prepared_query",
            Encode!(
                &name.to_string(),
                &Vec::<u8>::new(),
                &Option::<Vec<gleaph_prepared_api::PreparedSortSpec>>::None,
                &ReadMode::Eventual
            )
            .expect("encode prepared_query"),
        )
        .expect("prepared_query call");
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(_)) => panic!("prepared_query unexpectedly succeeded for {caller}"),
        Err(err) => panic!("decode prepared_query: {err}"),
    }
}

/// Raw `list_graph_grants` as the bootstrap admin (the fixture registry owner).
fn list_grants(env: &FederationEnv) -> Vec<GraphGrantSummary> {
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

#[test]
fn registration_denies_anonymous_until_bounded_publication_and_revoke_denies_again() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_two_persons_and_one_edge(&env);

    // Registration publishes nothing: anonymous is default-denied at the EXECUTE gate.
    prepare_batch_as_admin(
        &env,
        &[prepare_registration(
            "peoples",
            "MATCH (n:Person) RETURN n.name AS name",
        )],
    )
    .expect("register peoples");
    let err = prepared_query_err(&env, Principal::anonymous(), "peoples");
    assert!(matches!(err, RouterError::Forbidden), "got {err:?}");

    // Introspection surfaces the implicit-root marker even with zero stored graph rows
    // (ADR 0074 §3 invariant 3): the owner never sees an apparently empty list.
    let listed = list_grants(&env);
    assert_eq!(
        listed.len(),
        1,
        "exactly the implicit-root marker expected, got {listed:?}"
    );
    assert_eq!(listed[0].operation, GrantOperationView::ImplicitRoot);
    assert_eq!(listed[0].resource.kind, GrantResourceKindView::Graph);
    assert_eq!(
        listed[0].subject,
        GrantSubjectView::Principal(env.admin.to_text())
    );

    // Bounded publication admits the invocation only once the PUBLIC baseline also holds
    // the static requirements (SECURITY INVOKER: EXECUTE never carries scan privileges).
    publish_prepared_query_as(
        &env,
        env.admin,
        "peoples",
        "PUBLIC",
        "pub-lifecycle-publish",
    )
    .expect("owner publishes to PUBLIC");
    let err = prepared_query_err(&env, Principal::anonymous(), "peoples");
    assert!(matches!(err, RouterError::Forbidden), "got {err:?}");

    grant_public_data_rows(&env);
    let result = prepared_query_with_params_as(&env, Principal::anonymous(), "peoples", Vec::new());
    assert_eq!(result.row_count, 2, "both seeded persons expected");

    // Revocation denies again: exact-key removal of the targeted publication row.
    revoke_prepared_publication_as(&env, env.admin, "peoples", "PUBLIC", "pub-lifecycle-revoke")
        .expect("owner revokes the publication");
    let err = prepared_query_err(&env, Principal::anonymous(), "peoples");
    assert!(matches!(err, RouterError::Forbidden), "got {err:?}");
}

#[test]
fn granter_missing_one_requirement_cannot_publish() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);

    // Arm the PREPARE_REGISTER-only publisher (no tenancy, no data-plane rows).
    let granter = publisher();
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_grant_caps",
            Encode!(&GrantCapsArgs {
                target: granter,
                caps: 1, // PREPARE_REGISTER
            })
            .expect("encode admin_grant_caps"),
        )
        .expect("admin_grant_caps call");
    Decode!(&bytes, Result<(), RouterError>)
        .expect("decode admin_grant_caps")
        .expect("grant caps");

    prepare_batch_as_admin(
        &env,
        &[prepare_registration(
            "pattern",
            "MATCH (:Person)-[:KNOWS]->(:Person)",
        )],
    )
    .expect("register pattern");

    // Nothing covered: authority alone does not satisfy invariant 7.
    let err = publish_prepared_query_as(&env, granter, "pattern", "PUBLIC", "pub-invariant7-empty")
        .expect_err("uncovered granter must not publish");
    assert!(matches!(err, RouterError::Forbidden), "got {err:?}");
    // Fail-closed evidence: the rejected grant left no publication row behind — an
    // exact-key revoke misses with NotFound.
    let err = revoke_prepared_publication_as(
        &env,
        granter,
        "pattern",
        "PUBLIC",
        "pub-invariant7-probe-revoke",
    )
    .expect_err("no publication row may exist after a rejected grant");
    assert!(matches!(err, RouterError::NotFound(_)), "got {err:?}");

    // All-but-one covered (traversal still missing): still denied — invariant 7 requires
    // EVERY requirement of the stored static set, not merely some privilege.
    grant_read_rows_to(&env, &format!("PRINCIPAL '{}'", granter.to_text()));
    let err =
        publish_prepared_query_as(&env, granter, "pattern", "PUBLIC", "pub-invariant7-partial")
            .expect_err("partially covered granter must not publish");
    assert!(matches!(err, RouterError::Forbidden), "got {err:?}");

    // Full coverage: publication succeeds.
    gql_mutate_as_admin(
        &env,
        &format!(
            "GRANT TRAVERSE ON GRAPH {GRAPH_NAME} EDGES KNOWS TO PRINCIPAL '{}'",
            granter.to_text()
        ),
        "publication-grant-granter-traverse",
    );
    publish_prepared_query_as(
        &env,
        granter,
        "pattern",
        "PUBLIC",
        "pub-invariant7-complete",
    )
    .expect("fully covered granter publishes");
}

#[test]
fn replacing_a_record_cascades_stale_publications() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);

    // Publish a trivially-bounded query and arm the PUBLIC data-plane baseline.
    prepare_batch_as_admin(&env, &[prepare_registration("swap", "RETURN 'x' AS tag")])
        .expect("register swap");
    publish_prepared_query_as(&env, env.admin, "swap", "PUBLIC", "pub-cascade-first")
        .expect("publish light variant");
    grant_public_data_rows(&env);
    let result = prepared_query_with_params_as(&env, Principal::anonymous(), "swap", Vec::new());
    assert_eq!(result.row_count, 1);

    // Upsert the same name onto a heavier requirement set: the stale PUBLIC EXECUTE row
    // must die with the replaced record (invariant 7 across re-registration).
    prepare_batch_as_admin(
        &env,
        &[prepare_registration(
            "swap",
            "MATCH (n:Person) RETURN n.name AS name",
        )],
    )
    .expect("replace swap");
    let err = prepared_query_err(&env, Principal::anonymous(), "swap");
    assert!(
        matches!(err, RouterError::Forbidden),
        "stale publication must not survive the replacement, got {err:?}"
    );

    // Explicit re-publication against the new requirement set restores access.
    publish_prepared_query_as(&env, env.admin, "swap", "PUBLIC", "pub-cascade-second")
        .expect("re-publish heavy variant");
    let result = prepared_query_with_params_as(&env, Principal::anonymous(), "swap", Vec::new());
    assert_eq!(result.row_count, 0, "empty graph: zero rows but admitted");
}

#[test]
fn publication_survives_router_upgrade() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);

    prepare_batch_as_admin(
        &env,
        &[
            prepare_registration("upgrade_q", "RETURN 'kept' AS tag"),
            // Registered but never published: must stay denied across the upgrade.
            prepare_registration("private_q", "RETURN 'hidden' AS tag"),
        ],
    )
    .expect("register fixture queries");
    publish_prepared_query_as(
        &env,
        env.admin,
        "upgrade_q",
        "PUBLIC",
        "pub-upgrade-publish",
    )
    .expect("publish before upgrade");

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

    let result =
        prepared_query_with_params_as(&env, Principal::anonymous(), "upgrade_q", Vec::new());
    assert_eq!(result.row_count, 1, "the publication row must survive");

    // Default deny survives too: the never-published query stays denied for anonymous
    // callers (the record persisted; the upgrade did not blanket-admit anything).
    assert!(matches!(
        prepared_query_err(&env, Principal::anonymous(), "private_q"),
        RouterError::Forbidden
    ));

    // SECURITY INVOKER merging is `caller ∪ PUBLIC`: a named principal executes through
    // the same PUBLIC baseline — publication grants nothing extra to anyone else.
    let stranger = Principal::from_slice(&[0xE2; 29]);
    let result = prepared_query_with_params_as(&env, stranger, "upgrade_q", Vec::new());
    assert_eq!(result.row_count, 1);
}
