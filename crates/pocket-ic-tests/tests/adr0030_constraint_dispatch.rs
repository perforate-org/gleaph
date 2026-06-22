//! PocketIC: ADR 0030 — public `CREATE`/`DROP CONSTRAINT` dispatch seam.
//!
//! Slice 1 ships the constraint catalog and DDL parsing/storage but **no** write-path
//! enforcement. The public GQL dispatch must therefore:
//!   1. require admin/manager authorization (before doing anything else),
//!   2. reject the read entrypoint (write DDL on a `query` call), and
//!   3. refuse the operation with `NotImplemented` rather than publishing an unenforced
//!      uniqueness guarantee.
//!
//! This exercises the real ingress seam (Candid → `gql_execute_idempotent` / `gql_query`), not
//! just the parser/store units.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, gql_execute_idempotent_as_admin_expect_err, gql_query_as_admin_expect_err,
    install_two_graph_federation,
};

const CREATE: &str = "CREATE CONSTRAINT user_email FOR (n:User) REQUIRE n.email IS UNIQUE";
const DROP: &str = "DROP CONSTRAINT user_email";

/// Update-path `CREATE`/`DROP CONSTRAINT` by an authorized admin is recognized but refused with
/// `NotImplemented` until enforcement lands — never silently accepted.
#[test]
fn create_and_drop_constraint_dispatch_is_not_implemented() {
    let env = install_two_graph_federation();

    let create_err = gql_execute_idempotent_as_admin_expect_err(&env, CREATE, "adr0030_create");
    assert!(
        matches!(create_err, RouterError::NotImplemented(_)),
        "CREATE CONSTRAINT must be NotImplemented before enforcement, got {create_err:?}"
    );

    let drop_err = gql_execute_idempotent_as_admin_expect_err(&env, DROP, "adr0030_drop");
    assert!(
        matches!(drop_err, RouterError::NotImplemented(_)),
        "DROP CONSTRAINT must be NotImplemented before enforcement, got {drop_err:?}"
    );
}

/// Constraint DDL is a write program; running it on the `query` entrypoint is rejected with a
/// path mismatch before reaching the NotImplemented gate.
#[test]
fn create_constraint_on_query_entrypoint_is_path_mismatch() {
    let env = install_two_graph_federation();
    let err = gql_query_as_admin_expect_err(&env, CREATE);
    assert!(
        matches!(err, RouterError::ExecutionPathMismatch { .. }),
        "CREATE CONSTRAINT on the read path must be ExecutionPathMismatch, got {err:?}"
    );
}

/// Authorization is enforced ahead of everything else: a non-admin caller is rejected with
/// `Forbidden`, never reaching the path check or the NotImplemented gate.
#[test]
fn create_constraint_requires_authorization() {
    let env = install_two_graph_federation();
    let stranger = Principal::from_slice(&[0x11; 29]);
    assert_ne!(stranger, env.admin);

    let err = gql_execute_idempotent_expect_err_as(&env, stranger, CREATE, "adr0030_unauth");
    assert!(
        matches!(err, RouterError::Forbidden),
        "non-admin CREATE CONSTRAINT must be Forbidden, got {err:?}"
    );
}

/// `gql_execute_idempotent` as an arbitrary caller, expecting a `RouterError`.
fn gql_execute_idempotent_expect_err_as(
    env: &FederationEnv,
    caller: Principal,
    query: &str,
    client_mutation_key: &str,
) -> RouterError {
    let query = query.to_string();
    let params: Vec<u8> = Vec::new();
    let mutation_key = client_mutation_key.to_string();
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "gql_execute_idempotent",
            Encode!(&query, &params, &mutation_key).expect("encode gql_execute_idempotent"),
        )
        .unwrap_or_else(|e| panic!("gql_execute_idempotent on router: {e:?}"));
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(result)) => panic!("expected rejection, got row_count {}", result.row_count),
        Err(err) => panic!("decode gql_execute_idempotent: {err}"),
    }
}
