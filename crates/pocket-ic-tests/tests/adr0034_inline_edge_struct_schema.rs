//! PocketIC coverage for ADR 0034 Slice 24: fixed-size inline edge STRUCT schema registration.
//!
//! Router-owned DDL records the canonical named inline STRUCT slot. Graph receives only the
//! derived opaque `RawBytes` physical profile in this slice; struct reads, mutation packing, and
//! `COST BY` remain planned and are intentionally not exercised here.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, gql_execute_idempotent_as_admin, gql_execute_idempotent_as_admin_expect_err,
    install_single_shard_federation,
};

const EDGE_LABEL: &str = "AFFINITY";
const PROPERTY: &str = "stats";

fn inline_struct_ddl() -> String {
    format!(
        "CREATE EDGE LABEL {EDGE_LABEL} {{ {PROPERTY} STRUCT {{ score FLOAT32, confidence FLOAT32, updated_at UINT64 }} INLINE }}"
    )
}

fn scenario_unauthorized_ddl_is_forbidden_before_creation(env: &FederationEnv) {
    let query = "CREATE EDGE LABEL AFFINITY { stats STRUCT { score FLOAT32 } INLINE }";
    let params: Vec<u8> = Vec::new();
    let mutation_key = "adr0034_inline_struct_schema_unauthorized".to_string();
    let bytes = env
        .pic
        .update_call(
            env.router,
            Principal::anonymous(),
            "gql_execute_idempotent",
            Encode!(&query.to_string(), &params, &mutation_key)
                .expect("encode gql_execute_idempotent"),
        )
        .expect("router update call");
    let result = Decode!(&bytes,
        Result<GqlQueryResult, RouterError>)
    .expect("decode gql_execute_idempotent result");
    assert!(
        matches!(result, Err(RouterError::Forbidden)),
        "unauthorized scenario: anonymous caller should be forbidden before schema creation, got {result:?}"
    );
}

fn scenario_admin_creates_canonical_schema(env: &FederationEnv) {
    gql_execute_idempotent_as_admin(
        env,
        &inline_struct_ddl(),
        "adr0034_inline_struct_schema_create",
    );
}

fn scenario_exact_replay_is_idempotent(env: &FederationEnv) {
    gql_execute_idempotent_as_admin(
        env,
        &inline_struct_ddl(),
        "adr0034_inline_struct_schema_replay",
    );
}

fn scenario_conflicting_ddl_is_rejected(env: &FederationEnv) {
    let err = gql_execute_idempotent_as_admin_expect_err(
        env,
        "CREATE EDGE LABEL AFFINITY { stats STRUCT { score FLOAT64 } INLINE }",
        "adr0034_inline_struct_schema_conflict",
    );
    assert!(
        matches!(err, RouterError::Conflict(_)),
        "conflict scenario: expected Conflict, got {err:?}"
    );
}

fn scenario_scalar_ddl_after_struct_is_rejected(env: &FederationEnv) {
    let err = gql_execute_idempotent_as_admin_expect_err(
        env,
        "CREATE EDGE LABEL AFFINITY { stats FLOAT32 INLINE }",
        "adr0034_inline_struct_scalar_conflict",
    );
    assert!(
        matches!(err, RouterError::Conflict(_)),
        "scalar-after-struct scenario: expected Conflict, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// One ordered lifecycle fixture for the public struct schema registration path.
// ---------------------------------------------------------------------------

#[test]
fn inline_struct_schema_lifecycle() {
    let env = install_single_shard_federation();

    scenario_unauthorized_ddl_is_forbidden_before_creation(&env);
    scenario_admin_creates_canonical_schema(&env);
    scenario_exact_replay_is_idempotent(&env);
    scenario_conflicting_ddl_is_rejected(&env);
    scenario_scalar_ddl_after_struct_is_rejected(&env);
}
