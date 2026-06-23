//! PocketIC: ADR 0030 slice 8 — public `CREATE CONSTRAINT` publication (and `DROP` still gated).
//!
//! Slice 8 publishes `CREATE CONSTRAINT` over the real ingress seam (Candid →
//! `gql_execute_idempotent` / `gql_query`). The full enforcement lifecycle (Try/Acquire/Confirm,
//! Release, recovery, failure-injection) shipped in slices 5–7; this slice only opens the public
//! DDL surface. The dispatch must therefore:
//!   1. require admin/manager authorization (before doing anything else),
//!   2. reject the read entrypoint (write DDL on a `query` call),
//!   3. accept a well-formed `CREATE CONSTRAINT` (declare-on-empty) and then *enforce* it,
//!   4. surface store-level refusals precisely (`Conflict` for re-declare / non-empty label,
//!      `InvalidArgument` for malformed DDL), and
//!   5. still refuse `DROP CONSTRAINT` with `NotImplemented` (its draining lifecycle is a later
//!      slice — ADR 0030 Revision #15).

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, gql_execute_idempotent_as_admin, gql_execute_idempotent_as_admin_expect_err,
    gql_execute_idempotent_result_as_admin, gql_query_as_admin, gql_query_as_admin_expect_err,
    install_single_shard_federation,
};

const CONSTRAINT: &str = "user_email";
const LABEL: &str = "User";
const OTHER_LABEL: &str = "Member";
const PROPERTY: &str = "email";

fn create_for(label: &str) -> String {
    format!("CREATE CONSTRAINT {CONSTRAINT} FOR (n:{label}) REQUIRE n.{PROPERTY} IS UNIQUE")
}

fn insert_user(label: &str, email: &str) -> String {
    format!("INSERT (:{label} {{{PROPERTY}: '{email}'}})")
}

/// A well-formed `CREATE CONSTRAINT` by an authorized admin is accepted over the public ingress
/// (row_count 0) and is then *enforced*: a same-value duplicate `INSERT` is refused non-retryably.
#[test]
fn create_constraint_publishes_and_enforces() {
    let env = install_single_shard_federation();

    let created = gql_execute_idempotent_result_as_admin(&env, &create_for(LABEL), "s8-create");
    assert_eq!(
        created.row_count, 0,
        "CREATE CONSTRAINT is a write DDL that reports no rows"
    );

    gql_execute_idempotent_as_admin(&env, &insert_user(LABEL, "a@example.com"), "s8-ins-1");
    let live = gql_query_as_admin(&env, &format!("MATCH (n:{LABEL}) RETURN n"));
    assert_eq!(live.row_count, 1, "the constrained vertex committed");

    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_user(LABEL, "a@example.com"),
        "s8-ins-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "the published constraint is enforced: duplicate must be UniquenessViolation, got {dup:?}"
    );
}

/// Re-declaring an existing constraint conflicts; `IF NOT EXISTS` makes the declaration idempotent.
#[test]
fn duplicate_create_constraint_conflicts_unless_if_not_exists() {
    let env = install_single_shard_federation();

    gql_execute_idempotent_as_admin(&env, &create_for(LABEL), "s8-dup-1");

    let again = gql_execute_idempotent_as_admin_expect_err(&env, &create_for(LABEL), "s8-dup-2");
    assert!(
        matches!(again, RouterError::Conflict(_)),
        "re-declaring an existing constraint must Conflict, got {again:?}"
    );

    // `IF NOT EXISTS` is idempotent: a second declaration of the same name is accepted as a no-op.
    let idempotent = gql_execute_idempotent_result_as_admin(
        &env,
        &format!(
            "CREATE CONSTRAINT {CONSTRAINT} IF NOT EXISTS FOR (n:{LABEL}) REQUIRE n.{PROPERTY} IS UNIQUE"
        ),
        "s8-dup-ifne",
    );
    assert_eq!(
        idempotent.row_count, 0,
        "IF NOT EXISTS re-declaration is an accepted no-op"
    );
}

/// Declare-on-empty contract: a constraint over a label that already exists (a vertex of that label
/// was committed first) is refused with `Conflict` — uniqueness can only be declared on a new label.
#[test]
fn create_constraint_on_existing_label_is_rejected() {
    let env = install_single_shard_federation();

    // Seed a vertex under `OTHER_LABEL` so the label is interned before the constraint is declared.
    gql_execute_idempotent_as_admin(
        &env,
        &insert_user(OTHER_LABEL, "seed@example.com"),
        "s8-seed",
    );

    let err =
        gql_execute_idempotent_as_admin_expect_err(&env, &create_for(OTHER_LABEL), "s8-nonempty");
    assert!(
        matches!(err, RouterError::Conflict(_)),
        "CREATE CONSTRAINT on an existing label must Conflict (declare-on-empty), got {err:?}"
    );
}

/// Malformed constraint DDL is rejected with a precise `InvalidArgument`, not an opaque refusal.
#[test]
fn malformed_create_constraint_is_invalid_argument() {
    let env = install_single_shard_federation();
    // REQUIRE variable does not match the FOR pattern variable.
    let err = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &format!("CREATE CONSTRAINT {CONSTRAINT} FOR (n:{LABEL}) REQUIRE m.{PROPERTY} IS UNIQUE"),
        "s8-malformed",
    );
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "malformed CREATE CONSTRAINT must be InvalidArgument, got {err:?}"
    );
}

/// An unsupported edge uniqueness constraint is rejected over the public ingress with a precise
/// `InvalidArgument` (first-cut supports vertex single-property uniqueness only — ADR 0030), not a
/// fall-through to generic GQL or `NotImplemented`.
#[test]
fn edge_create_constraint_is_invalid_argument() {
    let env = install_single_shard_federation();
    let err = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &format!("CREATE CONSTRAINT {CONSTRAINT} FOR ()-[r:KNOWS]-() REQUIRE r.weight IS UNIQUE"),
        "s8-edge",
    );
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "edge CREATE CONSTRAINT must be InvalidArgument over the public ingress, got {err:?}"
    );
}

/// Constraint DDL is a write program; running `CREATE` on the `query` entrypoint is rejected with a
/// path mismatch before reaching the publication path.
#[test]
fn create_constraint_on_query_entrypoint_is_path_mismatch() {
    let env = install_single_shard_federation();
    let err = gql_query_as_admin_expect_err(&env, &create_for(LABEL));
    assert!(
        matches!(err, RouterError::ExecutionPathMismatch { .. }),
        "CREATE CONSTRAINT on the read path must be ExecutionPathMismatch, got {err:?}"
    );
}

/// Authorization is enforced ahead of everything else: a non-admin caller is rejected with
/// `Forbidden`, never reaching the path check or the publication path — and, because the guard runs
/// before any catalog mutation, the rejection leaves no constraint behind (an admin can subsequently
/// declare the same name as a fresh constraint, which would `Conflict` had the unauthorized attempt
/// registered anything).
#[test]
fn create_constraint_requires_authorization() {
    let env = install_single_shard_federation();
    let stranger = Principal::from_slice(&[0x11; 29]);
    assert_ne!(stranger, env.admin);

    let err = gql_execute_idempotent_expect_err_as(&env, stranger, &create_for(LABEL), "s8-unauth");
    assert!(
        matches!(err, RouterError::Forbidden),
        "non-admin CREATE CONSTRAINT must be Forbidden, got {err:?}"
    );

    // The guard ran before any store mutation: the same admin CREATE now succeeds (no pre-existing
    // constraint to Conflict with), proving the rejected attempt registered no catalog state.
    let created =
        gql_execute_idempotent_result_as_admin(&env, &create_for(LABEL), "s8-unauth-admin");
    assert_eq!(
        created.row_count, 0,
        "admin CREATE after a rejected non-admin attempt succeeds — no catalog state was left"
    );
}

/// `DROP CONSTRAINT` stays publicly `NotImplemented`: its reservation-draining lifecycle is a
/// separate slice (ADR 0030 Revision #15), so it must never be silently accepted — and the refused
/// DROP must not weaken the existing constraint, which keeps enforcing uniqueness.
#[test]
fn drop_constraint_is_not_implemented() {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(&env, &create_for(LABEL), "s8-drop-seed");

    let err = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &format!("DROP CONSTRAINT {CONSTRAINT}"),
        "s8-drop",
    );
    assert!(
        matches!(err, RouterError::NotImplemented(_)),
        "DROP CONSTRAINT must remain NotImplemented until its lifecycle slice, got {err:?}"
    );

    // The refused DROP left the constraint intact: a first INSERT commits, a same-value second is
    // still rejected non-retryably.
    gql_execute_idempotent_as_admin(
        &env,
        &insert_user(LABEL, "drop@example.com"),
        "s8-drop-ins-1",
    );
    let dup = gql_execute_idempotent_as_admin_expect_err(
        &env,
        &insert_user(LABEL, "drop@example.com"),
        "s8-drop-ins-dup",
    );
    assert!(
        matches!(dup, RouterError::UniquenessViolation(_)),
        "the constraint still enforces after a refused DROP: duplicate must be UniquenessViolation, got {dup:?}"
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
