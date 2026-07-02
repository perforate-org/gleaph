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
    e2e_insert_directed_edge_with_payload, e2e_insert_vertex_with_label,
    gql_execute_idempotent_as_admin, gql_query_as_admin, gql_query_as_admin_expect_err,
    install_single_shard_federation,
};
use std::collections::BTreeMap;

const EDGE_LABEL: &str = "ROAD";
const PROPERTY: &str = "distance";
const SRC_LABEL: &str = "CitySrc";
const MID_LABEL: &str = "CityMid";
const DST_LABEL: &str = "CityDst";

fn road_profile() -> gleaph_graph_kernel::entry::EdgePayloadProfile {
    gleaph_graph_kernel::entry::EdgePayloadProfile {
        byte_width: 2,
        encoding: gleaph_graph_kernel::entry::EdgePayloadEncoding::RawU16,
    }
}

fn setup() -> FederationEnv {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(
        &env,
        &format!("CREATE EDGE LABEL {EDGE_LABEL} {{ {PROPERTY} UINT16 INLINE }}"),
        "adr0034_inline_scalar_cost_by_schema_edge",
    );
    // Vertex labels are created implicitly through admin_intern_vertex_label in each test.
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

#[test]
fn inline_cost_by_selects_cheapest_path_not_fewest_hops() {
    let env = setup();
    let edge_label_id = admin_intern_edge_label(&env, EDGE_LABEL);
    let src_label_id = admin_intern_vertex_label(&env, SRC_LABEL);
    let mid_label_id = admin_intern_vertex_label(&env, MID_LABEL);
    let dst_label_id = admin_intern_vertex_label(&env, DST_LABEL);

    let a = e2e_insert_vertex_with_label(&env, env.graph_source, src_label_id.raw());
    let b = e2e_insert_vertex_with_label(&env, env.graph_source, mid_label_id.raw());
    let c = e2e_insert_vertex_with_label(&env, env.graph_source, dst_label_id.raw());

    // a->b costs 1, b->c costs 1: total 2.
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        a.local_vertex_id,
        b.local_vertex_id,
        edge_label_id.raw(),
        1u16.to_le_bytes().to_vec(),
        road_profile(),
    );
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        b.local_vertex_id,
        c.local_vertex_id,
        edge_label_id.raw(),
        1u16.to_le_bytes().to_vec(),
        road_profile(),
    );
    // Direct a->c costs 100.
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        a.local_vertex_id,
        c.local_vertex_id,
        edge_label_id.raw(),
        100u16.to_le_bytes().to_vec(),
        road_profile(),
    );

    let c_id = vertex_element_id(&env, DST_LABEL);
    let result = gql_query_as_admin(
        &env,
        "MATCH p = ANY SHORTEST (a:CitySrc)-[e:ROAD]->{1,5}(c) COST BY e.distance RETURN p, ELEMENT_ID(c) AS cid",
    );
    let rows = extract_rows(result);
    let c_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.get("cid") == Some(&c_id))
        .collect();
    assert_eq!(
        c_rows.len(),
        1,
        "expected exactly one shortest path ending at the CityDst vertex"
    );
    assert_eq!(
        path_element_count(c_rows[0], "p"),
        5,
        "expected a->b->c (2 hops) to beat direct a->c (1 hop) on total cost"
    );
}

#[test]
fn inline_cost_by_observes_mutation() {
    let env = setup();
    let edge_label_id = admin_intern_edge_label(&env, EDGE_LABEL);
    let src_label_id = admin_intern_vertex_label(&env, SRC_LABEL);
    let mid_label_id = admin_intern_vertex_label(&env, MID_LABEL);
    let dst_label_id = admin_intern_vertex_label(&env, DST_LABEL);

    let a = e2e_insert_vertex_with_label(&env, env.graph_source, src_label_id.raw());
    let b = e2e_insert_vertex_with_label(&env, env.graph_source, mid_label_id.raw());
    let c = e2e_insert_vertex_with_label(&env, env.graph_source, dst_label_id.raw());

    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        a.local_vertex_id,
        b.local_vertex_id,
        edge_label_id.raw(),
        1u16.to_le_bytes().to_vec(),
        road_profile(),
    );
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        b.local_vertex_id,
        c.local_vertex_id,
        edge_label_id.raw(),
        1u16.to_le_bytes().to_vec(),
        road_profile(),
    );
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        a.local_vertex_id,
        c.local_vertex_id,
        edge_label_id.raw(),
        100u16.to_le_bytes().to_vec(),
        road_profile(),
    );

    let c_id = vertex_element_id(&env, DST_LABEL);
    let cheapest = |env: &FederationEnv| {
        let result = gql_query_as_admin(
            env,
            "MATCH p = ANY SHORTEST (a:CitySrc)-[e:ROAD]->{1,5}(c) COST BY e.distance RETURN p, ELEMENT_ID(c) AS cid",
        );
        let rows = extract_rows(result);
        let c_rows: Vec<_> = rows
            .iter()
            .filter(|row| row.get("cid") == Some(&c_id))
            .collect();
        assert_eq!(c_rows.len(), 1);
        path_element_count(c_rows[0], "p")
    };

    assert_eq!(cheapest(&env), 5, "initial cheapest path is a->b->c");

    // Raise the cost of edge a->b using an inline-property mutation.
    gql_execute_idempotent_as_admin(
        &env,
        "MATCH (a:CitySrc)-[e:ROAD {distance: 1}]->(b) SET e.distance = 200 RETURN e",
        "adr0034_inline_cost_by_mutation",
    );

    assert_eq!(
        cheapest(&env),
        3,
        "expected direct a->c to become cheapest after raising a->b cost"
    );
}

#[test]
fn inline_cost_by_rejects_missing_inline_property() {
    let env = setup();
    let edge_label_id = admin_intern_edge_label(&env, EDGE_LABEL);
    let src_label_id = admin_intern_vertex_label(&env, SRC_LABEL);
    let dst_label_id = admin_intern_vertex_label(&env, DST_LABEL);

    let a = e2e_insert_vertex_with_label(&env, env.graph_source, src_label_id.raw());
    let c = e2e_insert_vertex_with_label(&env, env.graph_source, dst_label_id.raw());
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        a.local_vertex_id,
        c.local_vertex_id,
        edge_label_id.raw(),
        1u16.to_le_bytes().to_vec(),
        road_profile(),
    );

    let err = gql_query_as_admin_expect_err(
        &env,
        "MATCH p = ANY SHORTEST (a:CitySrc)-[e:ROAD]->{1,5}(c) COST BY e.missing RETURN p, ELEMENT_ID(c) AS cid",
    );
    assert!(
        matches!(
            err,
            RouterError::InvalidArgument(_) | RouterError::NotFound(_)
        ),
        "expected planning/execution failure for missing inline cost property, got {err:?}"
    );
}

#[test]
fn inline_cost_by_symmetric_directed_reads_same_payload_value() {
    let env = setup();
    let edge_label_id = admin_intern_edge_label(&env, EDGE_LABEL);
    let src_label_id = admin_intern_vertex_label(&env, SRC_LABEL);
    let mid_label_id = admin_intern_vertex_label(&env, MID_LABEL);

    let a = e2e_insert_vertex_with_label(&env, env.graph_source, src_label_id.raw());
    let b = e2e_insert_vertex_with_label(&env, env.graph_source, mid_label_id.raw());
    // Model a symmetric edge pair (both directions) sharing the same inline payload.
    // A true single undirected inline-payload edge is covered by a Graph unit test;
    // this E2E test proves the cost value is read from the payload in both traversal directions.
    let payload = 5u16.to_le_bytes().to_vec();
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        a.local_vertex_id,
        b.local_vertex_id,
        edge_label_id.raw(),
        payload.clone(),
        road_profile(),
    );
    e2e_insert_directed_edge_with_payload(
        &env,
        env.graph_source,
        b.local_vertex_id,
        a.local_vertex_id,
        edge_label_id.raw(),
        payload,
        road_profile(),
    );

    let b_id = vertex_element_id(&env, MID_LABEL);
    let result = gql_query_as_admin(
        &env,
        "MATCH p = ANY SHORTEST (a:CitySrc)-[e:ROAD]-{1,5}(b) COST BY e.distance RETURN p, ELEMENT_ID(b) AS bid",
    );
    let rows = extract_rows(result);
    let b_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.get("bid") == Some(&b_id))
        .collect();
    assert_eq!(
        b_rows.len(),
        1,
        "expected exactly one shortest path ending at the CityMid vertex"
    );
    assert_eq!(path_element_count(b_rows[0], "p"), 3);
}
