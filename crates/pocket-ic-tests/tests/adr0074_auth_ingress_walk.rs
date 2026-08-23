//! PocketIC: ADR 0074 slice-1 adversarial ingress walk over the Router authorization surface.
//!
//! One bounded environment walks every authorization-sensitive handler family and asserts
//! **default deny** for an adversarial caller plus **one success path**, per ADR 0074
//! Migration ("adversarial tests must walk the ingress surface asserting deny-by-default
//! plus one success path per handler family"). The anonymous public-prepared-execution flow
//! is proven end-to-end through the registration-time PUBLIC EXECUTE auto-seed.
//!
//! Handler families covered:
//! 1. Ad-hoc GQL (`gql_query`) — anonymous denied, bootstrap admin succeeds.
//! 2. Prepared registration (`prepare`) — anonymous denied, admin succeeds.
//! 3. Prepared execution (`prepared_query`) — anonymous executes ONLY via the PUBLIC seed;
//!    an unknown query name stays denied (the seed, not a residual default, admits it).
//! 4. Caps administration (`admin_grant_caps`, `my_caps`) — a caller without
//!    `MANAGE_AUTHORIZATION` is denied; admin grants a narrow cap that then takes effect.
//! 5. Graph topology (`register_graph`) — anonymous denied; the fixture's successful
//!    registration doubles as the success path.
//! 6. Index DDL (`CREATE INDEX` via `gql_mutate`) — anonymous denied; admin succeeds.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::{GqlQueryResult, ReadMode};
use gleaph_pocket_ic_tests::{
    FederationEnv, ensure_property, ensure_vertex_label, gql_mutate_result_as_admin, gql_query_as,
    install_single_shard_federation, prepare_batch_as_admin, prepared_query_with_params_as,
};
use gleaph_prepared_api::PreparedRegistration;
use gleaph_router::types::{GrantCapsArgs, RegisterGraphArgs, RegisterGraphShard};

/// An unauthenticated caller: holds no stored caps row and is a tenant of no graph.
fn adversarial_caller() -> Principal {
    Principal::anonymous()
}

/// Raw `prepare` returning the typed error so denials are observable.
fn prepare_err(
    env: &FederationEnv,
    caller: Principal,
    operations: Vec<PreparedRegistration>,
) -> RouterError {
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "prepare",
            Encode!(&operations).expect("encode prepare"),
        )
        .expect("prepare call");
    match Decode!(&bytes, Result<(), RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(())) => panic!("prepare unexpectedly succeeded for {caller}"),
        Err(err) => panic!("decode prepare: {err}"),
    }
}

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

/// Raw `admin_grant_caps` returning the typed error so denials are observable.
fn grant_caps_err(env: &FederationEnv, caller: Principal, args: GrantCapsArgs) -> RouterError {
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "admin_grant_caps",
            Encode!(&args).expect("encode admin_grant_caps"),
        )
        .expect("admin_grant_caps call");
    match Decode!(&bytes, Result<(), RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(())) => panic!("admin_grant_caps unexpectedly succeeded for {caller}"),
        Err(err) => panic!("decode admin_grant_caps: {err}"),
    }
}

/// Raw `register_graph` returning the typed error so denials are observable.
fn register_graph_err(env: &FederationEnv, caller: Principal) -> RouterError {
    let args = RegisterGraphArgs {
        graph_name: "walk.deny".into(),
        owner: caller,
        admins: Default::default(),
        is_home: false,
        shards: vec![RegisterGraphShard {
            shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
            graph_canister: Principal::management_canister(),
            index_canister: env.index,
        }],
        requested_resources: Vec::new(),
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "register_graph",
            Encode!(&args).expect("encode register_graph"),
        )
        .expect("register_graph call");
    match Decode!(&bytes, Result<(), RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(())) => panic!("register_graph unexpectedly succeeded for {caller}"),
        Err(err) => panic!("decode register_graph: {err}"),
    }
}

/// Raw `gql_mutate` returning the typed error so denials are observable.
fn gql_mutate_err(env: &FederationEnv, caller: Principal, query: &str) -> RouterError {
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "gql_mutate",
            Encode!(
                &query.to_string(),
                &Vec::<u8>::new(),
                &"walk-deny-key".to_string()
            )
            .expect("encode gql_mutate"),
        )
        .expect("gql_mutate call");
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(_)) => panic!("gql_mutate unexpectedly succeeded for {caller}"),
        Err(err) => panic!("decode gql_mutate: {err}"),
    }
}

fn my_caps_of(env: &FederationEnv, caller: Principal) -> Vec<String> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "my_caps",
            Encode!().expect("encode my_caps"),
        )
        .expect("my_caps call");
    Decode!(&bytes, Result<Vec<String>, RouterError>)
        .expect("decode my_caps")
        .expect("my_caps")
}

#[test]
fn ingress_walk_default_deny_plus_one_success_per_handler_family() {
    let env = install_single_shard_federation();
    let anon = adversarial_caller();

    // --- 1. Ad-hoc GQL ---
    // Default deny: anonymous holds no caps and is a tenant of no graph. The denial carries
    // the ADR 0028 non-disclosure shape (NotFound), never a distinguishable authorization
    // signal about graph existence.
    let adhoc = gql_query_as(&env, anon, "MATCH (n) RETURN n").expect_err("anonymous denied");
    assert!(matches!(adhoc, RouterError::NotFound(_)));
    // Success path: the bootstrap admin (full caps) runs the same read.
    let admin_read = gql_query_as(&env, env.admin, "MATCH (n) RETURN n").expect("admin read");
    assert_eq!(admin_read.row_count, 0);

    // --- 2. Prepared registration ---
    let reg = PreparedRegistration {
        // Constant-only body: under slice-2b plan-time enforcement an anonymous caller
        // holds no data-plane rows, so the published operation deliberately reads no
        // graph elements; the family tests EXECUTE-publication, not scan semantics.
        name: "walk-q".into(),
        query: "RETURN 'walk' AS tag".into(),
        metadata: None,
    };
    let prep_err = prepare_err(&env, anon, vec![reg.clone()]);
    assert!(matches!(prep_err, RouterError::Forbidden));
    // Success path: admin registers; this co-writes the PUBLIC EXECUTE seed (family 3).
    prepare_batch_as_admin(&env, &[reg]).expect("admin registers prepared query");

    // --- 3. Prepared execution ---
    // Unknown names stay default-deny even for anonymous callers: proves the PUBLIC seed —
    // not any residual Executor-style default — is what admits execution.
    let missing = prepared_query_err(&env, anon, "never-registered-walk-q");
    assert!(matches!(missing, RouterError::NotFound(_)));
    // Success path: the registered query executes anonymously through the PUBLIC seed.
    // A constant-only RETURN yields exactly its single literal row.
    let result = prepared_query_with_params_as(&env, anon, "walk-q", Vec::new());
    assert_eq!(result.row_count, 1);

    // --- 4. Caps administration ---
    let stranger = Principal::from_slice(&[0x5A; 29]);
    let grant_err = grant_caps_err(
        &env,
        stranger,
        GrantCapsArgs {
            target: stranger,
            caps: u64::MAX,
        },
    );
    assert!(matches!(grant_err, RouterError::NotAuthorized));
    // Success path: admin grants a narrow cap; introspection shows exactly that cap.
    const INDEX_CREATE_BITS: u64 = 1 << 1;
    grant_caps_ok(&env, stranger, INDEX_CREATE_BITS);
    assert_eq!(my_caps_of(&env, stranger), vec!["INDEX_CREATE".to_string()]);

    // --- 5. Graph topology ---
    let topo_err = register_graph_err(&env, anon);
    assert!(matches!(topo_err, RouterError::NotAuthorized));
    // Success path: `install_single_shard_federation` already registered `{GRAPH_NAME}` as
    // the bootstrap admin and every family above ran against it.

    // --- 6. Index DDL ---
    let ddl = "CREATE INDEX walk_idx IF NOT EXISTS FOR (n:WalkLabel) ON (n.walk_prop)";
    let ddl_err = gql_mutate_err(&env, anon, ddl);
    assert!(matches!(ddl_err, RouterError::Forbidden));
    // Success path: admin creates the same index (label/property ensured first).
    ensure_vertex_label(&env, "WalkLabel");
    ensure_property(&env, "walk_prop");
    let row_count = gql_mutate_result_as_admin(&env, ddl, "walk-success-key").row_count;
    assert_eq!(row_count, 0);
}

fn grant_caps_ok(env: &FederationEnv, target: Principal, caps: u64) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_grant_caps",
            Encode!(&GrantCapsArgs { target, caps }).expect("encode admin_grant_caps"),
        )
        .expect("admin_grant_caps call");
    match Decode!(&bytes, Result<(), RouterError>) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("admin_grant_caps rejected: {err:?}"),
        Err(err) => panic!("decode admin_grant_caps: {err}"),
    }
}
