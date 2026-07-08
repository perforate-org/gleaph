//! PocketIC coverage for ADR 0034 Slice 23: ordinary `COST BY e.property` shortest-path cost.
//!
//! Router-resolved schema identifies the named inline scalar property; Graph evaluates each hop
//! through the shared inline-aware edge property reader while preserving `WeightedCost` validation,
//! ordering, and accumulation. The compatibility surface `GLEAPH.COST BY GLEAPH.WEIGHT(e)` remains
//! unchanged.

use gleaph_gql_ic::{IcWirePlanQueryResult, IcWireValue};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, admin_intern_edge_label, admin_intern_vertex_label,
    e2e_insert_directed_edge_with_inline_value, e2e_insert_vertex_with_label,
    gql_execute_idempotent_as_admin, gql_query_as_admin, gql_query_as_admin_expect_err,
    install_single_shard_federation,
};
use std::collections::BTreeMap;

const EDGE_LABEL: &str = "ROAD";
const PROPERTY: &str = "distance";
const SRC_LABEL: &str = "CitySrc";
const MID_LABEL: &str = "CityMid";
const DST_LABEL: &str = "CityDst";

const COST_BY_QUERY: &str = "MATCH p = ANY SHORTEST (a:CitySrc)-[e:ROAD]->{1,5}(c) COST BY e.distance RETURN p, ELEMENT_ID(c) AS cid";

fn road_profile() -> gleaph_graph_kernel::entry::EdgeInlineValueProfile {
    gleaph_graph_kernel::entry::EdgeInlineValueProfile {
        byte_width: 2,
        encoding: gleaph_graph_kernel::entry::EdgeInlineValueEncoding::RawU16,
    }
}

fn setup() -> FederationEnv {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(
        &env,
        &format!("CREATE EDGE LABEL {EDGE_LABEL} {{ {PROPERTY} UINT16 INLINE }}"),
        "adr0034_inline_scalar_cost_by_schema_edge",
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

fn path_element_count(row: &BTreeMap<String, IcWireValue>, column: &str) -> usize {
    match row.get(column) {
        Some(IcWireValue::Path(elements)) => elements.len(),
        other => panic!("expected path column {column}, got {other:?}"),
    }
}

fn vertex_element_id(env: &FederationEnv, label: &str) -> IcWireValue {
    let rows = extract_rows(gql_query_as_admin(
        env,
        &format!("MATCH (v:{label}) RETURN ELEMENT_ID(v) AS v_id"),
    ));
    assert_eq!(rows.len(), 1, "expected exactly one {label} vertex");
    rows[0].get("v_id").cloned().expect("ELEMENT_ID(v) column")
}

fn build_cost_triangle(env: &FederationEnv) -> IcWireValue {
    let edge_label_id = admin_intern_edge_label(env, EDGE_LABEL);
    let src_label_id = admin_intern_vertex_label(env, SRC_LABEL);
    let mid_label_id = admin_intern_vertex_label(env, MID_LABEL);
    let dst_label_id = admin_intern_vertex_label(env, DST_LABEL);

    let a = e2e_insert_vertex_with_label(env, env.graph_source, src_label_id.raw());
    let b = e2e_insert_vertex_with_label(env, env.graph_source, mid_label_id.raw());
    let c = e2e_insert_vertex_with_label(env, env.graph_source, dst_label_id.raw());

    // a->b costs 1, b->c costs 1: total 2.
    e2e_insert_directed_edge_with_inline_value(
        env,
        env.graph_source,
        a.local_vertex_id,
        b.local_vertex_id,
        edge_label_id.raw(),
        1u16.to_le_bytes().to_vec(),
        road_profile(),
    );
    e2e_insert_directed_edge_with_inline_value(
        env,
        env.graph_source,
        b.local_vertex_id,
        c.local_vertex_id,
        edge_label_id.raw(),
        1u16.to_le_bytes().to_vec(),
        road_profile(),
    );
    // Direct a->c costs 100.
    e2e_insert_directed_edge_with_inline_value(
        env,
        env.graph_source,
        a.local_vertex_id,
        c.local_vertex_id,
        edge_label_id.raw(),
        100u16.to_le_bytes().to_vec(),
        road_profile(),
    );

    vertex_element_id(env, DST_LABEL)
}

fn cheapest_path_element_count(env: &FederationEnv, dst_id: &IcWireValue) -> usize {
    let rows = extract_rows(gql_query_as_admin(env, COST_BY_QUERY));
    let c_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.get("cid") == Some(dst_id))
        .collect();
    assert_eq!(
        c_rows.len(),
        1,
        "expected exactly one shortest path ending at the CityDst vertex"
    );
    path_element_count(c_rows[0], "p")
}

fn scenario_initial_cheapest_path_is_two_hops(env: &FederationEnv, dst_id: &IcWireValue) {
    assert_eq!(
        cheapest_path_element_count(env, dst_id),
        5,
        "initial cheapest path is a->b->c (cost 2), not direct a->c (cost 100)"
    );
}

fn scenario_missing_cost_property_rejects_and_leaves_graph_unchanged(
    env: &FederationEnv,
    dst_id: &IcWireValue,
) {
    // Former contract: inline_cost_by_rejects_missing_inline_property.
    let err = gql_query_as_admin_expect_err(
        env,
        "MATCH p = ANY SHORTEST (a:CitySrc)-[e:ROAD]->{1,5}(c) COST BY e.missing RETURN p, ELEMENT_ID(c) AS cid",
    );
    assert!(
        matches!(
            err,
            RouterError::InvalidArgument(_) | RouterError::NotFound(_)
        ),
        "expected planning/execution failure for missing inline cost property, got {err:?}"
    );

    // Bounded postcondition: returning the error must not have mutated graph state.
    assert_eq!(
        cheapest_path_element_count(env, dst_id),
        5,
        "after missing-property rejection the valid two-hop cheapest path must still exist"
    );
}

fn scenario_mutation_switches_to_direct_path(env: &FederationEnv, dst_id: &IcWireValue) {
    // Former contract: inline_cost_by_observes_mutation.
    gql_execute_idempotent_as_admin(
        env,
        "MATCH (a:CitySrc)-[e:ROAD {distance: 1}]->(b) SET e.distance = 200 RETURN e",
        "adr0034_inline_cost_by_lifecycle_mutation",
    );
    assert_eq!(
        cheapest_path_element_count(env, dst_id),
        3,
        "direct a->c must become cheapest after raising a->b cost"
    );
}

// ---------------------------------------------------------------------------
// Fixture family 1: full cheapest-path lifecycle over the same triangle.
// Former contracts preserved:
//   - inline_cost_by_selects_cheapest_path_not_fewest_hops
//   - inline_cost_by_observes_mutation
//   - inline_cost_by_rejects_missing_inline_property
// ---------------------------------------------------------------------------

#[test]
fn inline_cost_by_lifecycle() {
    let env = setup();
    let c_id = build_cost_triangle(&env);

    scenario_initial_cheapest_path_is_two_hops(&env, &c_id);
    scenario_missing_cost_property_rejects_and_leaves_graph_unchanged(&env, &c_id);
    scenario_mutation_switches_to_direct_path(&env, &c_id);
}

// ---------------------------------------------------------------------------
// Fixture family 2: symmetric directed pair proves inline value is read in both
// traversal directions.
//
// Two opposite directed edges (CitySrc->CityMid and CityMid->CitySrc) share the
// same inline value.  We run two explicit directed COST BY queries so each
// traversal direction is independently observable; an undirected query would
// allow either edge to satisfy the path and would not test both directions.
// Former contract preserved:
//   - inline_cost_by_symmetric_directed_reads_same_inline_value
// ---------------------------------------------------------------------------

fn assert_directed_cost_by_path(
    env: &FederationEnv,
    query: &str,
    dst_id_column: &str,
    expected_dst: &IcWireValue,
    direction: &str,
) {
    let rows = extract_rows(gql_query_as_admin(env, query));
    let dst_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.get(dst_id_column) == Some(expected_dst))
        .collect();
    assert_eq!(
        dst_rows.len(),
        1,
        "expected exactly one shortest path ending at the {direction} destination"
    );
    assert_eq!(
        path_element_count(dst_rows[0], "p"),
        3,
        "expected a single directed hop {direction} to contain three path elements"
    );
}

#[test]
fn inline_cost_by_symmetric_directed_reads_same_inline_value() {
    let env = setup();
    let edge_label_id = admin_intern_edge_label(&env, EDGE_LABEL);
    let src_label_id = admin_intern_vertex_label(&env, SRC_LABEL);
    let mid_label_id = admin_intern_vertex_label(&env, MID_LABEL);

    let a = e2e_insert_vertex_with_label(&env, env.graph_source, src_label_id.raw());
    let b = e2e_insert_vertex_with_label(&env, env.graph_source, mid_label_id.raw());

    // Model a symmetric edge pair (both directions) sharing the same inline value.
    // A true single undirected inline-payload edge is covered by a Graph unit test;
    // this E2E test proves the cost value is read from the payload in each
    // traversal direction independently.  Only the start vertex is labeled in each
    // query so the router seed-anchor prefix binds a single variable; the exact
    // destination is asserted via the returned ELEMENT_ID.
    let payload = 5u16.to_le_bytes().to_vec();
    e2e_insert_directed_edge_with_inline_value(
        &env,
        env.graph_source,
        a.local_vertex_id,
        b.local_vertex_id,
        edge_label_id.raw(),
        payload.clone(),
        road_profile(),
    );
    e2e_insert_directed_edge_with_inline_value(
        &env,
        env.graph_source,
        b.local_vertex_id,
        a.local_vertex_id,
        edge_label_id.raw(),
        payload,
        road_profile(),
    );

    let a_id = vertex_element_id(&env, SRC_LABEL);
    let b_id = vertex_element_id(&env, MID_LABEL);

    assert_directed_cost_by_path(
        &env,
        "MATCH p = ANY SHORTEST (a:CitySrc)-[e:ROAD]->{1,5}(b) COST BY e.distance RETURN p, ELEMENT_ID(b) AS bid",
        "bid",
        &b_id,
        "CitySrc -> CityMid",
    );

    assert_directed_cost_by_path(
        &env,
        "MATCH p = ANY SHORTEST (b:CityMid)-[e:ROAD]->{1,5}(a) COST BY e.distance RETURN p, ELEMENT_ID(a) AS aid",
        "aid",
        &a_id,
        "CityMid -> CitySrc",
    );
}
