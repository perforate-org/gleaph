//! PocketIC coverage for ADR 0034 Slice 20: scalar inline edge-property schema registration.
//!
//! Router-owned DDL records the canonical named inline slot; Graph consumes the derived physical
//! profile on the existing payload execution path. This E2E uses `UINT16 INLINE` because
//! `GLEAPH.WEIGHT(e)` only decodes legacy 2-byte weight encodings in Slice 20.
//! Ordinary `e.distance` property access remains out of scope.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::entry::{EdgeLabelId, EdgePayloadEncoding, EdgePayloadProfile};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, admin_intern_edge_label, e2e_insert_directed_edge_with_payload,
    e2e_insert_vertex, gql_execute_idempotent_as_admin, gql_execute_idempotent_as_admin_expect_err,
    gql_query_as_admin, install_single_shard_federation,
};

const EDGE_LABEL: &str = "ROAD";
const PROPERTY: &str = "distance";

fn inline_ddl() -> String {
    format!("CREATE EDGE LABEL {EDGE_LABEL} {{ {PROPERTY} UINT16 INLINE }}")
}

fn road_payload(value: u16) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

fn road_profile() -> EdgePayloadProfile {
    EdgePayloadProfile {
        byte_width: 2,
        encoding: EdgePayloadEncoding::WeightRawU16,
    }
}

fn setup() -> FederationEnv {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(&env, &inline_ddl(), "adr0034_inline_scalar_schema_create");
    env
}

#[test]
fn authorized_inline_scalar_ddl_creates_schema() {
    let env = setup();
    let label_id = admin_intern_edge_label(&env, "ROAD");
    assert_eq!(label_id, EdgeLabelId::from_raw(label_id.raw()));
}

#[test]
fn exact_inline_ddl_replay_is_idempotent() {
    let env = setup();
    gql_execute_idempotent_as_admin(&env, &inline_ddl(), "adr0034_inline_scalar_schema_replay");
}

#[test]
fn conflicting_inline_scalar_ddl_is_rejected() {
    let env = setup();
    let err = gql_execute_idempotent_as_admin_expect_err(
        &env,
        "CREATE EDGE LABEL ROAD { distance FLOAT64 INLINE }",
        "adr0034_inline_scalar_schema_conflict",
    );
    assert!(
        matches!(err, RouterError::Conflict(_)),
        "expected Conflict, got {err:?}"
    );
}

#[test]
fn unauthorized_inline_scalar_ddl_is_forbidden() {
    let env = install_single_shard_federation();
    let query = inline_ddl();
    let params: Vec<u8> = Vec::new();
    let mutation_key = "adr0034_inline_scalar_schema_unauthorized".to_string();
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
        Result<GqlQueryResult, RouterError>
    )
    .expect("decode gql_execute_idempotent result");
    assert!(
        matches!(result, Err(RouterError::Forbidden)),
        "anonymous caller should be forbidden, got {result:?}"
    );
}

#[test]
fn inline_schema_derived_profile_feeds_edge_payload_predicate() {
    let env = setup();
    let label_id = admin_intern_edge_label(&env, "ROAD");
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        label_id.raw(),
        road_payload(3),
        road_profile(),
    );

    // Slice 20 does not lower `e.distance`; use the existing `GLEAPH.WEIGHT` compatibility surface.
    let result = gql_query_as_admin(
        &env,
        "MATCH (a)-[e:ROAD]->(b) WHERE GLEAPH.WEIGHT(e) = 3 RETURN b",
    );
    assert_eq!(
        result.row_count, 1,
        "edge-payload predicate should see the 2-byte weight payload as 3"
    );
}

#[test]
fn width_mismatch_payload_rejects_insert() {
    let env = setup();
    let label_id = admin_intern_edge_label(&env, "ROAD");
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);

    let args = gleaph_pocket_ic_tests::E2eInsertDirectedEdgeWithPayloadArgs {
        source_local_vertex_id: source.local_vertex_id,
        target_local_vertex_id: target.local_vertex_id,
        edge_label_id: label_id.raw(),
        payload: vec![0u8, 0, 0, 0], // 4 bytes, does not match UINT16 profile
        payload_profile: road_profile(),
    };
    let bytes = env
        .pic
        .update_call(
            env.graph_source,
            env.router,
            "e2e_insert_directed_edge_with_payload",
            Encode!(&args).expect("encode e2e_insert_directed_edge_with_payload"),
        )
        .expect("graph update call");
    let result = Decode!(&bytes,
        Result<(), String>
    )
    .expect("decode e2e_insert_directed_edge_with_payload result");
    assert!(
        result.is_err(),
        "4-byte payload must be rejected against a 2-byte UINT16/WeightRawU16 profile, got {result:?}"
    );
}
