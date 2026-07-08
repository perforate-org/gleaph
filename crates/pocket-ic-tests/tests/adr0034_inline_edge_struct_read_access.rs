//! PocketIC coverage for ADR 0034 Slice 25: ordinary read access to fixed-size inline edge STRUCTs.
//!
//! Router-resolved schema identifies the named inline STRUCT slot; Graph decodes the edge inline value
//! into a GQL record so `e.stats.field` works in projection, filtering, aggregate input, and
//! ordering. The inline slot is the only read source for its `(label, property)` pair; a sidecar
//! value cannot override it.
//!
//! All scenarios run inside one fresh PocketIC fixture with distinct source-vertex labels so results
//! remain independent.

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

const EDGE_LABEL: &str = "AFFINITY";
const PROPERTY: &str = "stats";

fn inline_struct_ddl() -> String {
    format!(
        "CREATE EDGE LABEL {EDGE_LABEL} {{ {PROPERTY} {{ score FLOAT32, confidence FLOAT32, updated_at UINT64 }} INLINE }}"
    )
}

fn affinity_profile() -> gleaph_graph_kernel::entry::EdgeInlineValueProfile {
    gleaph_graph_kernel::entry::EdgeInlineValueProfile {
        byte_width: 16,
        encoding: gleaph_graph_kernel::entry::EdgeInlineValueEncoding::RawBytes,
    }
}

fn pack_stats_payload(score: f32, confidence: f32, updated_at: u64) -> Vec<u8> {
    let mut payload = Vec::with_capacity(16);
    payload.extend_from_slice(&score.to_le_bytes());
    payload.extend_from_slice(&confidence.to_le_bytes());
    payload.extend_from_slice(&updated_at.to_le_bytes());
    payload
}

fn setup() -> FederationEnv {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(
        &env,
        &inline_struct_ddl(),
        "adr0034_inline_struct_read_access_schema",
    );
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

fn insert_affinity(
    env: &FederationEnv,
    source: u32,
    target: u32,
    label_id: u16,
    score: f32,
    confidence: f32,
    updated_at: u64,
) {
    e2e_insert_directed_edge_with_inline_value(
        env,
        env.graph_source,
        source,
        target,
        label_id,
        pack_stats_payload(score, confidence, updated_at),
        affinity_profile(),
    );
}

fn scenario_projection_returns_struct_fields(env: &FederationEnv, label_id: u16) {
    let source_label = admin_intern_vertex_label(env, "ProjectionSource").raw();
    let source = e2e_insert_vertex_with_label(env, env.graph_source, source_label).local_vertex_id;
    let target = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    insert_affinity(env, source, target, label_id, 3.5, 0.75, 1_700_000_000);

    let result = gql_query_as_admin(
        env,
        "MATCH (a:ProjectionSource)-[e:AFFINITY]->(b) RETURN e.stats.score AS s, e.stats.confidence AS c, e.stats.updated_at AS u",
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "projection scenario: expected one AFFINITY edge"
    );
    assert_eq!(
        rows[0].get("s"),
        Some(&IcWireValue::Float64(3.5)),
        "projection scenario: score field must decode"
    );
    assert_eq!(
        rows[0].get("c"),
        Some(&IcWireValue::Float64(0.75)),
        "projection scenario: confidence field must decode"
    );
    assert_eq!(
        rows[0].get("u"),
        Some(&IcWireValue::Uint64(1_700_000_000)),
        "projection scenario: updated_at field must decode"
    );
}

fn scenario_filter_matches_struct_field(env: &FederationEnv, label_id: u16) {
    let source_label = admin_intern_vertex_label(env, "FilterSource").raw();
    let source = e2e_insert_vertex_with_label(env, env.graph_source, source_label).local_vertex_id;
    let match_target = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    let skip_target = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    insert_affinity(
        env,
        source,
        match_target,
        label_id,
        3.5,
        0.75,
        1_700_000_000,
    );
    insert_affinity(env, source, skip_target, label_id, 2.0, 0.50, 1_700_000_001);

    let result = gql_query_as_admin(
        env,
        "MATCH (a:FilterSource)-[e:AFFINITY]->(b) WHERE e.stats.score >= 3.0 RETURN e.stats.score AS s",
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "filter scenario: expected one edge with score >= 3.0"
    );
    assert_eq!(
        rows[0].get("s"),
        Some(&IcWireValue::Float64(3.5)),
        "filter scenario: wrong edge selected"
    );
}

fn scenario_order_by_sorts_by_struct_field(env: &FederationEnv, label_id: u16) {
    let source_label = admin_intern_vertex_label(env, "OrderSource").raw();
    let source = e2e_insert_vertex_with_label(env, env.graph_source, source_label).local_vertex_id;
    let first = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    let second = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    // Insert out of order to prove ORDER BY reads the payload, not insertion order.
    insert_affinity(env, source, second, label_id, 2.0, 0.50, 1_700_000_001);
    insert_affinity(env, source, first, label_id, 3.5, 0.75, 1_700_000_000);

    let result = gql_query_as_admin(
        env,
        "MATCH (a:OrderSource)-[e:AFFINITY]->(b) RETURN e.stats.score AS s ORDER BY s ASC",
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 2, "order scenario: expected two edges");
    assert_eq!(
        rows[0].get("s"),
        Some(&IcWireValue::Float64(2.0)),
        "order scenario: first row must be the smaller score"
    );
    assert_eq!(
        rows[1].get("s"),
        Some(&IcWireValue::Float64(3.5)),
        "order scenario: second row must be the larger score"
    );
}

fn scenario_aggregate_uses_struct_field(env: &FederationEnv, label_id: u16) {
    let source_label = admin_intern_vertex_label(env, "AggSource").raw();
    let source = e2e_insert_vertex_with_label(env, env.graph_source, source_label).local_vertex_id;
    let a = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    let b = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    insert_affinity(env, source, a, label_id, 3.5, 0.75, 1_700_000_000);
    insert_affinity(env, source, b, label_id, 2.5, 0.60, 1_700_000_001);

    let result = gql_query_as_admin(
        env,
        "MATCH (a:AggSource)-[e:AFFINITY]->(b) RETURN AVG(e.stats.score) AS avg_score",
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "aggregate scenario: expected one aggregate row"
    );
    let avg = rows[0].get("avg_score").expect("avg_score");
    let expected = IcWireValue::Float64(3.0);
    assert_eq!(
        avg, &expected,
        "aggregate scenario: AVG of 3.5 and 2.5 must be 3.0"
    );
}

fn scenario_payload_wins_over_sidecar(env: &FederationEnv, label_id: u16) {
    let source_label = admin_intern_vertex_label(env, "PrecedenceSource").raw();
    let source = e2e_insert_vertex_with_label(env, env.graph_source, source_label).local_vertex_id;
    let target = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    insert_affinity(env, source, target, label_id, 3.5, 0.75, 1_700_000_000);

    // Write a sidecar value with the same property id; the inline inline value must still win.
    let property_id = admin_intern_property(env, PROPERTY).raw();
    e2e_set_edge_property(env, env.graph_source, source, target, property_id, 99);

    let result = gql_query_as_admin(
        env,
        "MATCH (a:PrecedenceSource)-[e:AFFINITY]->(b) RETURN e.stats.score AS s",
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "precedence scenario: expected exactly one AFFINITY edge"
    );
    assert_eq!(
        rows[0].get("s"),
        Some(&IcWireValue::Float64(3.5)),
        "precedence scenario: inline value must win over sidecar value"
    );
}

fn scenario_unknown_struct_field_returns_null(env: &FederationEnv, label_id: u16) {
    let source_label = admin_intern_vertex_label(env, "UnknownSource").raw();
    let source = e2e_insert_vertex_with_label(env, env.graph_source, source_label).local_vertex_id;
    let target = e2e_insert_vertex(env, env.graph_source).local_vertex_id;
    insert_affinity(env, source, target, label_id, 3.5, 0.75, 1_700_000_000);

    let result = gql_query_as_admin(
        env,
        "MATCH (a:UnknownSource)-[e:AFFINITY]->(b) RETURN e.stats.missing AS m",
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1, "unknown-field scenario: expected one row");
    assert_eq!(
        rows[0].get("m"),
        Some(&IcWireValue::Null),
        "unknown-field scenario: missing struct field must be NULL"
    );
}

fn scenario_struct_index_create_rejects_inline_property(env: &FederationEnv) {
    let err = gql_execute_idempotent_as_admin_expect_err(
        env,
        "CREATE INDEX stats_idx FOR ()-[e:AFFINITY]-() ON (e.stats)",
        "adr0034_inline_struct_read_index_conflict",
    );
    assert!(
        matches!(err, RouterError::Conflict(_)),
        "index-conflict scenario: expected Conflict, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// One ordered fixture family covering Slice 25 read semantics end-to-end.
// ---------------------------------------------------------------------------

#[test]
fn inline_struct_read_access_suite() {
    let env = setup();
    let affinity_label_id = admin_intern_edge_label(&env, EDGE_LABEL).raw();

    scenario_projection_returns_struct_fields(&env, affinity_label_id);
    scenario_filter_matches_struct_field(&env, affinity_label_id);
    scenario_order_by_sorts_by_struct_field(&env, affinity_label_id);
    scenario_aggregate_uses_struct_field(&env, affinity_label_id);
    scenario_payload_wins_over_sidecar(&env, affinity_label_id);
    scenario_unknown_struct_field_returns_null(&env, affinity_label_id);
    scenario_struct_index_create_rejects_inline_property(&env);
}
