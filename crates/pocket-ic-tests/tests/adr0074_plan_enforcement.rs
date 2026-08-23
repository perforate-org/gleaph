//! PocketIC: ADR 0074 slice 2b — plan-time data-plane enforcement end to end.
//!
//! One suite proves that fine-grained grants gate query execution on the Router:
//!
//! 1. A grantee holding MATCH + READ + READ_PROPERTY(name) on Person plus
//!    TRAVERSE OUTGOING on KNOWS runs the forward pattern and gets real values back;
//!    the reverse pattern fails with the uniform Forbidden; projecting the ungranted
//!    `secret` property fails the whole query (never NULL-substituted).
//! 2. The registry owner is unaffected (ownership-derived coverage).
//! 3. A capability holder without grants or tenancy cannot even resolve the graph —
//!    caps never imply data access (ADR 0028 indistinguishable NotFound at the
//!    visibility gate, uniform Forbidden at plan time for visible-but-uncovered).
//! 4. PUBLIC data-plane rows enable anonymous ad-hoc reads.
//! 5. Prepared execution: registration publishes nothing (slice 3); a bounded PUBLIC
//!    EXECUTE row admits the invocation but never carries scan privileges — the static
//!    requirement set still demands the PUBLIC data-plane rows.
//! 6. Enforcement state and outcomes survive a canister upgrade.
//!
//! Expected counts stated before running (per plan contract):
//! - 4 `#[test]` functions.
//! - Each test constructs one PocketIC environment (`new_pocket_ic`) and installs exactly
//!   3 canisters: Router, Index, Graph shard (`install_single_shard_federation`);
//!   canister *creation* calls are 3 funded `create_canister` per environment.
//! - Update/query call budget per environment: 1 schema-bind mutate, 3 seed mutates with
//!   3 maintenance drains, 4–8 GRANT mutates plus ≤1 bounded EXECUTE publication,
//!   ≤1 `admin_grant_caps`, ≤1 `prepare`, ≤1 upgrade triple, and ≤12 probe calls.
//! - Regression reruns required alongside: `adr0074_grant_grammar`,
//!   `adr0074_auth_ingress_walk`, `smoke`.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, drain_maintenance_via_timer, gql_mutate_as_admin, gql_query_as,
    gql_query_as_admin, install_single_shard_federation, prepare_batch_as_admin,
    prepared_query_with_params_as, publish_prepared_query_as, wasm_bytes,
};
use gleaph_router::types::GrantCapsArgs;

fn grantee() -> Principal {
    Principal::from_slice(&[0xD1; 29])
}

/// A capability holder with no tenancy and no data-plane grant rows.
fn caps_holder() -> Principal {
    Principal::from_slice(&[0xD2; 29])
}

/// Bind the typed schema: Person{name, secret} and a DIRECTED KNOWS edge.
fn bind_typed_schema(env: &FederationEnv) {
    let ddl = format!(
        "CREATE GRAPH TYPE gt {{ NODE Person {{ name STRING, secret STRING }}, \
         DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person) }} \
         NEXT CREATE GRAPH {} TYPED gt",
        gleaph_pocket_ic_tests::GRAPH_NAME
    );
    gql_mutate_as_admin(env, &ddl, "plan-enforcement-bind-schema");
}

/// Seed alice -[KNOWS]-> bob, each a Person with a name and a secret.
fn seed_two_persons_and_one_edge(env: &FederationEnv) {
    gql_mutate_as_admin(
        env,
        "INSERT (:Person {name: 'alice', secret: 'sa'})",
        "plan-enforcement-seed-alice",
    );
    drain_maintenance_via_timer(env, env.graph_source);
    gql_mutate_as_admin(
        env,
        "INSERT (:Person {name: 'bob', secret: 'sb'})",
        "plan-enforcement-seed-bob",
    );
    drain_maintenance_via_timer(env, env.graph_source);
    gql_mutate_as_admin(
        env,
        "MATCH (a:Person {name: 'alice'}) RETURN a NEXT \
         MATCH (b:Person {name: 'bob'}) \
         INSERT (a)-[:KNOWS]->(b)",
        "plan-enforcement-seed-knows",
    );
    drain_maintenance_via_timer(env, env.graph_source);
}

const FORWARD_QUERY: &str = "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name AS name";
const REVERSE_QUERY: &str = "MATCH (a:Person)<-[:KNOWS]-(b:Person) RETURN b.name AS name";

fn grant_to(env: &FederationEnv, subject: &str) {
    let graph = gleaph_pocket_ic_tests::GRAPH_NAME;
    for statement in [
        format!("GRANT MATCH ON GRAPH {graph} NODES Person TO {subject}"),
        format!("GRANT READ ON GRAPH {graph} NODES Person TO {subject}"),
        format!("GRANT READ ON GRAPH {graph} NODES Person {{ name }} TO {subject}"),
        format!("GRANT TRAVERSE OUTGOING ON GRAPH {graph} EDGES KNOWS TO {subject}"),
    ] {
        gql_mutate_as_admin(env, &statement, "plan-enforcement-grant");
    }
}

/// Raw `gql_query` returning the typed error so denials are observable.
fn query_err(env: &FederationEnv, caller: Principal, query: &str) -> RouterError {
    use gleaph_graph_kernel::plan_exec::ReadMode;
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "gql_query",
            Encode!(&query.to_string(), &Vec::<u8>::new(), &ReadMode::Eventual)
                .expect("encode gql_query"),
        )
        .expect("gql_query call");
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(_)) => panic!("gql_query unexpectedly succeeded for {caller}: {query}"),
        Err(err) => panic!("decode gql_query: {err}"),
    }
}

fn returned_names(result: &GqlQueryResult) -> Vec<String> {
    use gleaph_gql::Value;
    use gleaph_gql_ic::GqlWireRows;
    let rows_blob = result.rows_blob.as_ref().expect("rows blob present");
    let wire = GqlWireRows::decode_blob(rows_blob).expect("decode rows");
    let mut names = Vec::new();
    for row in wire.rows {
        let value_row = row.try_into_value_row().expect("value row");
        let Value::Text(name) = value_row.get("name").expect("name column") else {
            panic!("expected text name, got {:?}", value_row.get("name"));
        };
        names.push(name.clone());
    }
    names.sort();
    names
}

#[test]
fn grantee_forward_admitted_reverse_and_secret_projection_denied_owner_unaffected() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_two_persons_and_one_edge(&env);
    grant_to(&env, &format!("PRINCIPAL '{}'", grantee().to_text()));

    // Forward pattern: admitted, with the authorized projection returning real values.
    let forward = gql_query_as(&env, grantee(), FORWARD_QUERY).expect("forward admitted");
    assert_eq!(forward.row_count, 1);
    assert_eq!(returned_names(&forward), vec!["bob".to_owned()]);

    // Reverse pattern: uniformly denied with the generic Forbidden — the error names
    // neither the missing privilege nor the resource.
    assert!(matches!(
        query_err(&env, grantee(), REVERSE_QUERY),
        RouterError::Forbidden
    ));

    // Unauthorized EXPLICIT property projection fails the whole query; it is never
    // substituted with NULL (ADR 0074 §16).
    assert!(matches!(
        query_err(&env, grantee(), "MATCH (b:Person) RETURN b.secret"),
        RouterError::Forbidden
    ));
    // The authorized projection keeps working unchanged after the denial.
    let again = gql_query_as(&env, grantee(), FORWARD_QUERY).expect("forward still admitted");
    assert_eq!(returned_names(&again), vec!["bob".to_owned()]);

    // The owner runs every shape without any grant row (ownership-derived coverage).
    let owner_reverse = gql_query_as_admin(&env, REVERSE_QUERY);
    assert_eq!(owner_reverse.row_count, 1, "owner reverse unaffected");

    // Caps never imply data access: a PREPARE_REGISTER-only principal has no tenancy and
    // no rows, so the graph stays invisible (ADR 0028 non-disclosure shape).
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_grant_caps",
            Encode!(&GrantCapsArgs {
                target: caps_holder(),
                caps: 1, // PREPARE_REGISTER
            })
            .expect("encode admin_grant_caps"),
        )
        .expect("admin_grant_caps call");
    Decode!(&bytes, Result<(), RouterError>)
        .expect("decode admin_grant_caps")
        .expect("grant caps ok");
    assert!(matches!(
        query_err(&env, caps_holder(), FORWARD_QUERY),
        RouterError::NotFound(_)
    ));
}

#[test]
fn public_rows_enable_anonymous_adhoc_reads() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_two_persons_and_one_edge(&env);

    // Before any PUBLIC row the anonymous caller cannot even resolve the graph.
    assert!(matches!(
        query_err(&env, Principal::anonymous(), FORWARD_QUERY),
        RouterError::NotFound(_)
    ));

    grant_to(&env, "PUBLIC");

    let forward = gql_query_as(&env, Principal::anonymous(), FORWARD_QUERY)
        .expect("anonymous read enabled by PUBLIC rows");
    assert_eq!(forward.row_count, 1);
    assert_eq!(returned_names(&forward), vec!["bob".to_owned()]);
    // The PUBLIC baseline gates exactly what it grants: reverse traversal denied.
    assert!(matches!(
        query_err(&env, Principal::anonymous(), REVERSE_QUERY),
        RouterError::Forbidden
    ));
}

#[test]
fn prepared_execution_inherits_plan_time_enforcement() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_two_persons_and_one_edge(&env);

    // Registration stores static requirements and publishes nothing (slice 3): an
    // anonymous caller is default-denied at the EXECUTE gate.
    prepare_batch_as_admin(
        &env,
        &[gleaph_prepared_api::PreparedRegistration {
            name: "peoples".into(),
            query: "MATCH (n:Person) RETURN n.name AS name".into(),
            metadata: None,
        }],
    )
    .expect("register prepared query");
    let err = prepared_query_err(&env, Principal::anonymous(), "peoples");
    assert!(matches!(err, RouterError::Forbidden), "got {err:?}");

    // Bounded publication admits EXECUTE only; the data-plane requirement set still
    // denies the invocation (EXECUTE alone never carries scan privileges).
    publish_prepared_query_as(
        &env,
        env.admin,
        "peoples",
        "PUBLIC",
        "plan-enforcement-publish-peoples",
    )
    .expect("owner publishes to PUBLIC");
    let err = prepared_query_err(&env, Principal::anonymous(), "peoples");
    assert!(matches!(err, RouterError::Forbidden), "got {err:?}");

    // Negative on the ad-hoc side too: none of this makes the graph itself visible.
    assert!(matches!(
        query_err(&env, Principal::anonymous(), FORWARD_QUERY),
        RouterError::NotFound(_)
    ));

    // Positive: once PUBLIC also holds the data-plane rows, the same invocation succeeds.
    grant_to(&env, "PUBLIC");
    let result = prepared_query_with_params_as(&env, Principal::anonymous(), "peoples", Vec::new());
    assert_eq!(result.row_count, 2);
    assert_eq!(
        returned_names(&result),
        vec!["alice".to_owned(), "bob".to_owned()]
    );
}

fn prepared_query_err(env: &FederationEnv, caller: Principal, name: &str) -> RouterError {
    use gleaph_graph_kernel::plan_exec::ReadMode;
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

#[test]
fn enforcement_outcomes_survive_upgrade() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_two_persons_and_one_edge(&env);
    grant_to(&env, &format!("PRINCIPAL '{}'", grantee().to_text()));

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

    // Grant rows survive (MemoryId 55) and enforcement outcomes are identical.
    let forward =
        gql_query_as(&env, grantee(), FORWARD_QUERY).expect("forward admitted post-upgrade");
    assert_eq!(returned_names(&forward), vec!["bob".to_owned()]);
    assert!(matches!(
        query_err(&env, grantee(), REVERSE_QUERY),
        RouterError::Forbidden
    ));
}
