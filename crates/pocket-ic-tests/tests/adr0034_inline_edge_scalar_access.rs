//! PocketIC coverage for ADR 0034 Slice 21: ordinary read access to a scalar inline edge property.
//!
//! Router-resolved schema identifies the named inline property; Graph decodes the bound edge inline value
//! into the exact GQL scalar value for projection, filtering, and ordering. The inline slot is the
//! only read source for its `(label, property)` pair; a sidecar value with the same property id
//! cannot override or rescue the read.
//!
//! All former standalone contracts run as named scenarios inside one fresh PocketIC fixture. Each
//! scenario owns a distinct source-vertex label so the read results remain independently observable
//! and cannot be confused by edges created for another scenario.

use gleaph_gql_ic::{IcWirePlanQueryResult, IcWireValue};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, admin_intern_edge_label, admin_intern_property, admin_intern_vertex_label,
    e2e_insert_directed_edge_with_inline_value, e2e_insert_vertex, e2e_insert_vertex_with_label,
    e2e_set_edge_property, gql_execute_idempotent_as_admin,
    gql_execute_idempotent_as_admin_expect_err, gql_query_as_admin,
    install_single_shard_federation,
};
use std::collections::BTreeMap;

const EDGE_LABEL: &str = "ROAD";
const PROPERTY: &str = "distance";

fn road_profile() -> gleaph_graph_kernel::entry::EdgeInlineValueProfile {
    gleaph_graph_kernel::entry::EdgeInlineValueProfile {
        byte_width: 2,
        encoding: gleaph_graph_kernel::entry::EdgeInlineValueEncoding::WeightRawU16,
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

fn insert_road(env: &FederationEnv, source: u32, target: u32, road_label_id: u16, distance: u16) {
    e2e_insert_directed_edge_with_inline_value(
        env,
        env.graph_source,
        source,
        target,
        road_label_id,
        distance.to_le_bytes().to_vec(),
        road_profile(),
    );
}

fn scenario_projection_returns_inline_value(env: &FederationEnv, road_label_id: u16) {
    let source_label = admin_intern_vertex_label(env, "ProjectionSource").raw();
    let source = e2e_insert_vertex_with_label(env, env.graph_source, source_label).local_vertex_id;
    let target = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    insert_road(env, source, target, road_label_id, 7);

    let result = gql_query_as_admin(
        env,
        "MATCH (a:ProjectionSource)-[e:ROAD]->(b) RETURN e.distance AS d",
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "projection scenario: expected exactly one ROAD edge from a ProjectionSource"
    );
    assert_eq!(
        rows[0].get("d"),
        Some(&IcWireValue::Uint64(7)),
        "projection scenario: inline value must be returned"
    );
}

fn scenario_filter_matches_inline_value(env: &FederationEnv, road_label_id: u16) {
    let source_label = admin_intern_vertex_label(env, "FilterSource").raw();
    let source = e2e_insert_vertex_with_label(env, env.graph_source, source_label).local_vertex_id;
    let match_target = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    let skip_target = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    insert_road(env, source, match_target, road_label_id, 7);
    insert_road(env, source, skip_target, road_label_id, 9);

    let result = gql_query_as_admin(
        env,
        "MATCH (a:FilterSource)-[e:ROAD]->(b) WHERE e.distance = 7 RETURN e.distance AS d",
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "filter scenario: expected exactly one matching edge with distance 7"
    );
    assert_eq!(
        rows[0].get("d"),
        Some(&IcWireValue::Uint64(7)),
        "filter scenario: must not select the edge with inline value 9"
    );
}

fn scenario_order_by_sorts_by_inline_value(env: &FederationEnv, road_label_id: u16) {
    let source_label = admin_intern_vertex_label(env, "OrderSource").raw();
    let source = e2e_insert_vertex_with_label(env, env.graph_source, source_label).local_vertex_id;
    let first = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    let second = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    // Insert out of order to prove ORDER BY reads payload, not insertion order.
    insert_road(env, source, second, road_label_id, 9);
    insert_road(env, source, first, road_label_id, 7);

    let result = gql_query_as_admin(
        env,
        "MATCH (a:OrderSource)-[e:ROAD]->(b) RETURN e.distance AS d ORDER BY d ASC",
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        2,
        "order scenario: expected exactly two ROAD edges from an OrderSource"
    );
    assert_eq!(
        rows[0].get("d"),
        Some(&IcWireValue::Uint64(7)),
        "order scenario: first row must be the smaller inline value"
    );
    assert_eq!(
        rows[1].get("d"),
        Some(&IcWireValue::Uint64(9)),
        "order scenario: second row must be the larger inline value"
    );
}

fn scenario_payload_wins_over_sidecar(env: &FederationEnv, road_label_id: u16) {
    let source_label = admin_intern_vertex_label(env, "PrecedenceSource").raw();
    let source = e2e_insert_vertex_with_label(env, env.graph_source, source_label).local_vertex_id;
    let target = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    insert_road(env, source, target, road_label_id, 7);

    // Write a sidecar value with the same property id; the inline inline value must still win.
    let property_id = admin_intern_property(env, PROPERTY).raw();
    e2e_set_edge_property(env, env.graph_source, source, target, property_id, 99);

    let result = gql_query_as_admin(
        env,
        "MATCH (a:PrecedenceSource)-[e:ROAD]->(b) RETURN e.distance AS d",
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "precedence scenario: expected exactly one ROAD edge from a PrecedenceSource"
    );
    assert_eq!(
        rows[0].get("d"),
        Some(&IcWireValue::Uint64(7)),
        "precedence scenario: inline value must win over sidecar value 99"
    );
}

fn scenario_edge_index_create_rejects_inline_property(env: &FederationEnv) {
    let err = gql_execute_idempotent_as_admin_expect_err(
        env,
        "CREATE INDEX dist_idx FOR ()-[e:ROAD]-() ON (e.distance)",
        "adr0034_inline_access_index_conflict",
    );
    assert!(
        matches!(err, RouterError::Conflict(_)),
        "index-conflict scenario: expected Conflict, got {err:?}"
    );
}

#[test]
fn inline_scalar_access_suite() {
    let env = setup();
    let road_label_id = admin_intern_edge_label(&env, EDGE_LABEL).raw();

    scenario_projection_returns_inline_value(&env, road_label_id);
    scenario_filter_matches_inline_value(&env, road_label_id);
    scenario_order_by_sorts_by_inline_value(&env, road_label_id);
    scenario_payload_wins_over_sidecar(&env, road_label_id);
    scenario_edge_index_create_rejects_inline_property(&env);
}
