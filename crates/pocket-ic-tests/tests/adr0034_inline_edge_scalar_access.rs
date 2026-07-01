//! PocketIC coverage for ADR 0034 Slice 21: ordinary read access to a scalar inline edge property.
//!
//! Router-resolved schema identifies the named inline property; Graph decodes the bound edge payload
//! into the exact GQL scalar value for projection, filtering, and ordering. The inline slot is the
//! only read source for its `(label, property)` pair; a sidecar value with the same property id
//! cannot override or rescue the read.

use gleaph_gql_ic::{IcWirePlanQueryResult, IcWireValue};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, admin_intern_edge_label, admin_intern_property,
    e2e_insert_directed_edge_with_payload, e2e_insert_vertex, e2e_set_edge_property,
    gql_execute_idempotent_as_admin, gql_execute_idempotent_as_admin_expect_err,
    gql_query_as_admin, install_single_shard_federation,
};
use std::collections::BTreeMap;

const EDGE_LABEL: &str = "ROAD";
const PROPERTY: &str = "distance";

fn road_profile() -> gleaph_graph_kernel::entry::EdgePayloadProfile {
    gleaph_graph_kernel::entry::EdgePayloadProfile {
        byte_width: 2,
        encoding: gleaph_graph_kernel::entry::EdgePayloadEncoding::WeightRawU16,
    }
}

fn inline_ddl() -> String {
    format!("CREATE EDGE LABEL {EDGE_LABEL} {{ {PROPERTY} UINT16 INLINE }}")
}

fn setup() -> FederationEnv {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(&env, &inline_ddl(), "adr0034_inline_scalar_access_schema");
    env
}

fn extract_rows(result: GqlQueryResult) -> Vec<BTreeMap<String, IcWireValue>> {
    let rows_blob = result.rows_blob.expect("rows blob");
    let wire = IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode rows");
    wire.rows
        .into_iter()
        .map(|row| row.columns.into_iter().collect())
        .collect()
}

#[test]
fn inline_property_projection_returns_payload_value() {
    let env = setup();
    let label_id = admin_intern_edge_label(&env, EDGE_LABEL);
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        label_id.raw(),
        7u16.to_le_bytes().to_vec(),
        road_profile(),
    );

    let result = gql_query_as_admin(&env, "MATCH (a)-[e:ROAD]->(b) RETURN e.distance AS d");
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("d"), Some(&IcWireValue::Uint64(7)));
}

#[test]
fn inline_property_filter_matches_payload_value() {
    let env = setup();
    let label_id = admin_intern_edge_label(&env, EDGE_LABEL);
    let source = e2e_insert_vertex(&env, env.graph_source);
    let match_target = e2e_insert_vertex(&env, env.graph_source);
    let skip_target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        source.local_vertex_id,
        match_target.local_vertex_id,
        label_id.raw(),
        7u16.to_le_bytes().to_vec(),
        road_profile(),
    );
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        source.local_vertex_id,
        skip_target.local_vertex_id,
        label_id.raw(),
        9u16.to_le_bytes().to_vec(),
        road_profile(),
    );

    let result = gql_query_as_admin(
        &env,
        "MATCH (a)-[e:ROAD]->(b) WHERE e.distance = 7 RETURN b",
    );
    assert_eq!(result.row_count, 1);
}

#[test]
fn inline_property_order_by_sorts_by_payload_value() {
    let env = setup();
    let label_id = admin_intern_edge_label(&env, EDGE_LABEL);
    let source = e2e_insert_vertex(&env, env.graph_source);
    let first = e2e_insert_vertex(&env, env.graph_source);
    let second = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        source.local_vertex_id,
        second.local_vertex_id,
        label_id.raw(),
        9u16.to_le_bytes().to_vec(),
        road_profile(),
    );
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        source.local_vertex_id,
        first.local_vertex_id,
        label_id.raw(),
        7u16.to_le_bytes().to_vec(),
        road_profile(),
    );

    let result = gql_query_as_admin(
        &env,
        "MATCH (a)-[e:ROAD]->(b) RETURN e.distance AS d ORDER BY d ASC",
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("d"), Some(&IcWireValue::Uint64(7)));
    assert_eq!(rows[1].get("d"), Some(&IcWireValue::Uint64(9)));
}

#[test]
fn inline_property_payload_wins_over_sidecar() {
    let env = setup();
    let label_id = admin_intern_edge_label(&env, EDGE_LABEL);
    let property_id = admin_intern_property(&env, PROPERTY).raw();
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        label_id.raw(),
        7u16.to_le_bytes().to_vec(),
        road_profile(),
    );
    // Write a sidecar value with the same property id; the inline payload must still win.
    e2e_set_edge_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        property_id,
        99,
    );

    let result = gql_query_as_admin(&env, "MATCH (a)-[e:ROAD]->(b) RETURN e.distance AS d");
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("d"), Some(&IcWireValue::Uint64(7)));
}

#[test]
fn edge_index_create_rejects_inline_property() {
    let env = setup();
    let err = gql_execute_idempotent_as_admin_expect_err(
        &env,
        "CREATE INDEX dist_idx FOR ()-[e:ROAD]-() ON (e.distance)",
        "adr0034_inline_access_index_conflict",
    );
    assert!(
        matches!(err, RouterError::Conflict(_)),
        "expected Conflict, got {err:?}"
    );
}
