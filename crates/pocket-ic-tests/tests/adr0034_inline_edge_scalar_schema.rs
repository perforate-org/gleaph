//! PocketIC coverage for ADR 0034 Slice 20: scalar inline edge-property schema registration.
//!
//! Router-owned DDL records the canonical named inline slot; Graph consumes the derived physical
//! profile on the existing inline property bytes execution path. This E2E uses `UINT16 INLINE` because
//! Ordinary `e.distance` property access uses the Router-resolved inline slot; the legacy
//! `GLEAPH.WEIGHT(e)` compatibility surface has been removed (ADR 0051 Phase B).
//!
//! All six former standalone contracts run as named, adversarially observable scenarios inside one
//! fresh PocketIC fixture. Unauthorized and conflicting DDL attempts happen before any inline property bytes work,
//! proving the canonical schema stays fail-closed.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::entry::{
    EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile, EdgeLabelId,
};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, e2e_insert_directed_edge_with_inline_property, e2e_insert_vertex,
    ensure_edge_label, gql_mutate_as_admin, gql_mutate_as_admin_expect_err, gql_query_as_admin,
    install_single_shard_federation,
};

const EDGE_LABEL: &str = "ROAD";
const PROPERTY: &str = "distance";

fn inline_ddl() -> String {
    format!(
        "CREATE GRAPH TYPE IF NOT EXISTS road_type {{ NODE City AS city, DIRECTED EDGE Road LABEL {EDGE_LABEL} {{ {PROPERTY} UINT16 INLINE }} CONNECTING (city -> city) }} NEXT CREATE GRAPH IF NOT EXISTS gleaph.pocket_ic TYPED road_type"
    )
}

fn road_inline_property_bytes(value: u16) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

fn road_profile() -> EdgeInlinePropertyProfile {
    EdgeInlinePropertyProfile {
        byte_width: 2,
        encoding: EdgeInlinePropertyEncoding::RawU16,
    }
}

// ---------------------------------------------------------------------------
// Scenario helpers: each preserves one former standalone contract as a named,
// independently diagnosable phase.
// ---------------------------------------------------------------------------

fn scenario_unauthorized_ddl_is_forbidden_before_creation(env: &FederationEnv) {
    // Attempt an adversarial type declaration before any authorized schema exists.
    // If this wrote anything to the catalog, the later canonical UINT16 creation would be
    // poisoned and fail observably.
    let query = "CREATE GRAPH TYPE IF NOT EXISTS road_type { NODE City AS city, DIRECTED EDGE Road LABEL ROAD { distance FLOAT64 INLINE } CONNECTING (city -> city) } NEXT CREATE GRAPH IF NOT EXISTS gleaph.pocket_ic TYPED road_type";
    let params: Vec<u8> = Vec::new();
    let mutation_key = "adr0034_inline_scalar_schema_unauthorized".to_string();
    let bytes = env
        .pic
        .update_call(
            env.router,
            Principal::anonymous(),
            "gql_mutate",
            Encode!(&query.to_string(), &params, &mutation_key).expect("encode gql_mutate"),
        )
        .expect("router update call");
    let result =
        Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_mutate result");
    assert!(
        matches!(result, Err(RouterError::Forbidden)),
        "unauthorized scenario: anonymous caller should be forbidden before schema creation, got {result:?}"
    );
}

fn scenario_admin_creates_canonical_schema(env: &FederationEnv) {
    gql_mutate_as_admin(env, &inline_ddl(), "adr0034_inline_scalar_schema_create");
    let label_id = ensure_edge_label(env, EDGE_LABEL);
    assert_eq!(label_id, EdgeLabelId::from_raw(label_id.raw()));
}

fn scenario_exact_replay_is_idempotent(env: &FederationEnv) {
    gql_mutate_as_admin(env, &inline_ddl(), "adr0034_inline_scalar_schema_replay");
}

fn scenario_conflicting_ddl_is_rejected(env: &FederationEnv) {
    let err = gql_mutate_as_admin_expect_err(
        env,
        "CREATE GRAPH TYPE IF NOT EXISTS road_type { NODE City AS city, DIRECTED EDGE Road LABEL ROAD { distance FLOAT64 INLINE } CONNECTING (city -> city) } NEXT CREATE GRAPH IF NOT EXISTS gleaph.pocket_ic TYPED road_type",
        "adr0034_inline_scalar_schema_conflict",
    );
    assert!(
        matches!(err, RouterError::Conflict(_)),
        "conflict scenario: expected Conflict, got {err:?}"
    );
}

fn scenario_derived_profile_feeds_inline_property_predicate(env: &FederationEnv) {
    let label_id = ensure_edge_label(env, EDGE_LABEL);
    let source = e2e_insert_vertex(env, env.graph_source);
    let target = e2e_insert_vertex(env, env.graph_source);
    e2e_insert_directed_edge_with_inline_property(
        env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        label_id.raw(),
        road_inline_property_bytes(3),
        road_profile(),
    );

    let result = gql_query_as_admin(env, "MATCH (a)-[e:ROAD]->(b) WHERE e.distance = 3 RETURN b");
    assert_eq!(
        result.row_count, 1,
        "predicate scenario: edge-inline-property-bytes predicate should see the 2-byte weight inline property bytes as 3"
    );
}

fn scenario_width_mismatch_rejects_insert(env: &FederationEnv) {
    let label_id = ensure_edge_label(env, EDGE_LABEL);
    let source = e2e_insert_vertex(env, env.graph_source);
    let target = e2e_insert_vertex(env, env.graph_source);

    let args = gleaph_pocket_ic_tests::E2eInsertDirectedEdgeWithInlinePropertyArgs {
        source_local_vertex_id: source.local_vertex_id,
        target_local_vertex_id: target.local_vertex_id,
        edge_label_id: label_id.raw(),
        inline_property_bytes: vec![0u8, 0, 0, 0], // 4 bytes, does not match UINT16 profile
        inline_property_profile: road_profile(),
    };
    let bytes = env
        .pic
        .update_call(
            env.graph_source,
            env.router,
            "e2e_insert_directed_edge_with_inline_property",
            Encode!(&args).expect("encode e2e_insert_directed_edge_with_inline_property"),
        )
        .expect("graph update call");
    let result = Decode!(&bytes, Result<(), String>)
        .expect("decode e2e_insert_directed_edge_with_inline_property result");
    assert!(
        result.is_err(),
        "width scenario: 4-byte inline property bytes must be rejected against a 2-byte UINT16/RawU16 profile, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// One ordered lifecycle fixture: fail-closed catalog behavior is verified by
// succeeding after both the unauthorized and the conflicting DDL attempts.
// ---------------------------------------------------------------------------

#[test]
fn inline_scalar_schema_lifecycle() {
    let env = install_single_shard_federation();

    scenario_unauthorized_ddl_is_forbidden_before_creation(&env);
    scenario_admin_creates_canonical_schema(&env);
    scenario_exact_replay_is_idempotent(&env);
    scenario_conflicting_ddl_is_rejected(&env);

    // Prove the canonical schema survived the adversarial attempts.
    scenario_derived_profile_feeds_inline_property_predicate(&env);
    scenario_width_mismatch_rejects_insert(&env);
}
