//! PocketIC coverage for ADR 0034 Slice 21: ordinary read access to a scalar inline edge property.
//!
//! Router-resolved schema identifies the named inline property; Graph decodes the bound edge inline property bytes
//! into the exact GQL scalar value for projection, filtering, and ordering. The inline slot is the
//! only read source for its `(label, property)` pair; a sidecar value with the same property id
//! cannot override or rescue the read.
//!
//! All scenarios run inside one fresh PocketIC fixture. Vertices carry the declared `City`
//! node label so every MATCH conforms to the typed schema's `CONNECTING (city -> city)`
//! constraint, and each scenario scopes its rows through a unique `distance` value band
//! instead of ad-hoc source labels.

use gleaph_gql_ic::{GqlWireRows, GqlWireValue};
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, e2e_insert_directed_edge_with_inline_property, e2e_insert_vertex_with_label,
    e2e_set_edge_property, ensure_edge_label, ensure_property, ensure_vertex_label,
    gql_mutate_as_admin, gql_query_as_admin, install_single_shard_federation,
};
use std::collections::BTreeMap;

const EDGE_LABEL: &str = "ROAD";
const PROPERTY: &str = "distance";
const NODE_LABEL: &str = "City";

/// One disjoint `distance` band per scenario keeps row sets independent inside the shared typed
/// store, replacing the former ad-hoc source-vertex labels.
const DISTANCE_PROJECTION: u16 = 7;
const DISTANCE_FILTER_MATCH: u16 = 17;
const DISTANCE_FILTER_SKIP: u16 = 19;
const DISTANCE_ORDER_LOW: u16 = 27;
const DISTANCE_ORDER_HIGH: u16 = 29;
const DISTANCE_PRECEDENCE: u16 = 37;

fn road_profile() -> gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
    gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
        byte_width: 2,
        encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
    }
}

fn inline_ddl() -> String {
    format!(
        "CREATE GRAPH TYPE IF NOT EXISTS road_type {{ NODE City AS city, DIRECTED EDGE Road LABEL {EDGE_LABEL} {{ {PROPERTY} UINT16 INLINE }} CONNECTING (city -> city) }} NEXT CREATE GRAPH IF NOT EXISTS gleaph.pocket_ic TYPED road_type"
    )
}

fn setup() -> FederationEnv {
    let env = install_single_shard_federation();
    gql_mutate_as_admin(&env, &inline_ddl(), "adr0034_inline_scalar_access_schema");
    env
}

fn extract_rows(result: GqlQueryResult) -> Vec<BTreeMap<String, GqlWireValue>> {
    let rows_blob = result.rows_blob.expect("rows blob");
    let wire = GqlWireRows::decode_blob(&rows_blob).expect("decode rows");
    wire.rows
        .into_iter()
        .map(|row| row.columns.into_iter().collect())
        .collect()
}

fn insert_road(env: &FederationEnv, source: u32, target: u32, road_label_id: u16, distance: u16) {
    e2e_insert_directed_edge_with_inline_property(
        env,
        env.graph_source,
        source,
        target,
        road_label_id,
        distance.to_le_bytes().to_vec(),
        road_profile(),
    );
}

/// Two fresh `City` vertices joined by one ROAD edge; returns `(source, target)`.
fn insert_city_pair(env: &FederationEnv, city_label_id: u16) -> (u32, u32) {
    let source = e2e_insert_vertex_with_label(env, env.graph_source, city_label_id).local_vertex_id;
    let target = e2e_insert_vertex_with_label(env, env.graph_source, city_label_id).local_vertex_id;
    (source, target)
}

fn scenario_projection_returns_inline_property(
    env: &FederationEnv,
    road_label_id: u16,
    city_label_id: u16,
) {
    let (source, target) = insert_city_pair(env, city_label_id);
    insert_road(env, source, target, road_label_id, DISTANCE_PROJECTION);

    let result = gql_query_as_admin(
        env,
        &format!(
            "MATCH (a:City)-[e:ROAD]->(b) WHERE e.distance = {DISTANCE_PROJECTION} RETURN e.distance AS d"
        ),
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "projection scenario: expected exactly one ROAD edge with the projected distance"
    );
    assert_eq!(
        rows[0].get("d"),
        Some(&GqlWireValue::Uint16(DISTANCE_PROJECTION)),
        "projection scenario: inline property bytes must be returned"
    );
}

fn scenario_filter_matches_inline_property(
    env: &FederationEnv,
    road_label_id: u16,
    city_label_id: u16,
) {
    let (source, match_target) = insert_city_pair(env, city_label_id);
    let (_, skip_target) = insert_city_pair(env, city_label_id);
    insert_road(
        env,
        source,
        match_target,
        road_label_id,
        DISTANCE_FILTER_MATCH,
    );
    insert_road(
        env,
        source,
        skip_target,
        road_label_id,
        DISTANCE_FILTER_SKIP,
    );

    let result = gql_query_as_admin(
        env,
        &format!(
            "MATCH (a:City)-[e:ROAD]->(b) WHERE e.distance = {DISTANCE_FILTER_MATCH} RETURN e.distance AS d"
        ),
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "filter scenario: expected exactly one matching edge with the filtered distance"
    );
    assert_eq!(
        rows[0].get("d"),
        Some(&GqlWireValue::Uint16(DISTANCE_FILTER_MATCH)),
        "filter scenario: must not select the edge with inline property bytes {DISTANCE_FILTER_SKIP}"
    );
}

fn scenario_order_by_sorts_by_inline_property(
    env: &FederationEnv,
    road_label_id: u16,
    city_label_id: u16,
) {
    let (source, first) = insert_city_pair(env, city_label_id);
    let (_, second) = insert_city_pair(env, city_label_id);
    // Insert out of order to prove ORDER BY reads inline property bytes, not insertion order.
    insert_road(env, source, second, road_label_id, DISTANCE_ORDER_HIGH);
    insert_road(env, source, first, road_label_id, DISTANCE_ORDER_LOW);

    let result = gql_query_as_admin(
        env,
        &format!(
            "MATCH (a:City)-[e:ROAD]->(b) WHERE e.distance >= {DISTANCE_ORDER_LOW} AND e.distance <= {DISTANCE_ORDER_HIGH} RETURN e.distance AS d ORDER BY d ASC"
        ),
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        2,
        "order scenario: expected exactly two ROAD edges from this scenario's distance band"
    );
    assert_eq!(
        rows[0].get("d"),
        Some(&GqlWireValue::Uint16(DISTANCE_ORDER_LOW)),
        "order scenario: first row must be the smaller inline property bytes"
    );
    assert_eq!(
        rows[1].get("d"),
        Some(&GqlWireValue::Uint16(DISTANCE_ORDER_HIGH)),
        "order scenario: second row must be the larger inline property bytes"
    );
}

fn scenario_inline_property_wins_over_sidecar(
    env: &FederationEnv,
    road_label_id: u16,
    city_label_id: u16,
) {
    let (source, target) = insert_city_pair(env, city_label_id);
    insert_road(env, source, target, road_label_id, DISTANCE_PRECEDENCE);

    // Write a sidecar value with the same property id; the inline inline property bytes must still win.
    let property_id = ensure_property(env, PROPERTY).raw();
    e2e_set_edge_property(env, env.graph_source, source, target, property_id, 99);

    let result = gql_query_as_admin(
        env,
        &format!(
            "MATCH (a:City)-[e:ROAD]->(b) WHERE e.distance = {DISTANCE_PRECEDENCE} RETURN e.distance AS d"
        ),
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "precedence scenario: expected exactly one ROAD edge in this scenario's distance band"
    );
    assert_eq!(
        rows[0].get("d"),
        Some(&GqlWireValue::Uint16(DISTANCE_PRECEDENCE)),
        "precedence scenario: inline property bytes must win over sidecar value 99"
    );
}

fn scenario_edge_index_create_allows_inline_scalar_property(env: &FederationEnv) {
    // Scalar inline slots stay indexable and index-maintained (`9e1967d57`); only inline STRUCT
    // slots conflict with edge indexes.
    gql_mutate_as_admin(
        env,
        "CREATE INDEX dist_idx FOR ()-[e:ROAD]-() ON (e.distance)",
        "adr0034_inline_access_index_over_inline_scalar",
    );
}

#[test]
fn inline_scalar_access_suite() {
    let env = setup();
    let road_label_id = ensure_edge_label(&env, EDGE_LABEL).raw();
    let city_label_id = ensure_vertex_label(&env, NODE_LABEL).raw();

    scenario_projection_returns_inline_property(&env, road_label_id, city_label_id);
    scenario_filter_matches_inline_property(&env, road_label_id, city_label_id);
    scenario_order_by_sorts_by_inline_property(&env, road_label_id, city_label_id);
    scenario_inline_property_wins_over_sidecar(&env, road_label_id, city_label_id);
    scenario_edge_index_create_allows_inline_scalar_property(&env);
}
