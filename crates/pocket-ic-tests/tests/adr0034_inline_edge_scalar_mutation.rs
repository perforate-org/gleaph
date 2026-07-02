//! PocketIC coverage for ADR 0034 Slice 22: ordinary GQL mutation packing into an inline scalar edge payload.
//!
//! Router-resolved schema identifies the named inline property. Graph evaluates and validates the
//! mutation value before writing, encodes it into the fixed-width payload, updates every physical
//! mirror of the logical edge, and never writes the matching property to the sidecar store.

use gleaph_gql_ic::{IcWirePlanQueryResult, IcWireValue};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, gql_execute_idempotent_as_admin, gql_execute_idempotent_as_admin_expect_err,
    gql_query_as_admin, install_single_shard_federation,
};
use std::collections::BTreeMap;

const EDGE_LABEL: &str = "ROAD";
const PROPERTY: &str = "distance";

fn inline_ddl() -> String {
    format!("CREATE EDGE LABEL {EDGE_LABEL} {{ {PROPERTY} UINT16 INLINE }}")
}

fn setup() -> FederationEnv {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(&env, &inline_ddl(), "adr0034_inline_scalar_mutation_schema");
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

// ---------------------------------------------------------------------------
// Scenario helpers: each preserves one former standalone contract as a named,
// independently diagnosable phase that can be composed into fixture families.
// ---------------------------------------------------------------------------

fn scenario_insert_round_trips_through_payload(
    env: &FederationEnv,
    value: u64,
    mutation_key: &str,
) {
    gql_execute_idempotent_as_admin(
        env,
        &format!("INSERT (:City)-[:ROAD {{{PROPERTY}: {value}}}]->(:City)"),
        mutation_key,
    );
}

fn scenario_assert_distance(env: &FederationEnv, expected: u64) {
    let result = gql_query_as_admin(
        env,
        "MATCH (a:City)-[e:ROAD]-(b:City) RETURN e.distance AS d",
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1, "expected exactly one ROAD edge");
    assert_eq!(
        rows[0].get("d"),
        Some(&IcWireValue::Uint64(expected)),
        "expected e.distance == {expected}"
    );
}

fn scenario_set_updates_payload_value(env: &FederationEnv, value: u64, mutation_key: &str) {
    gql_execute_idempotent_as_admin(
        env,
        &format!(
            "MATCH (a:City)-[e:ROAD]->(b:City) SET e.{PROPERTY} = {value} RETURN id(e) AS eid, e.{PROPERTY} AS d"
        ),
        mutation_key,
    );
}

fn scenario_remove_rejects_and_payload_unchanged(
    env: &FederationEnv,
    expected: u64,
    mutation_key: &str,
) {
    let err = gql_execute_idempotent_as_admin_expect_err(
        env,
        &format!("MATCH (a:City)-[e:ROAD]-(b:City) REMOVE e.{PROPERTY}"),
        mutation_key,
    );
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "expected REMOVE of inline property to fail with InvalidArgument, got {err:?}"
    );
    scenario_assert_distance(env, expected);
}

fn scenario_assert_no_road_edge(env: &FederationEnv) {
    let result = gql_query_as_admin(env, "MATCH (a:City)-[e:ROAD]-(b:City) RETURN e");
    assert_eq!(
        result.row_count, 0,
        "expected no ROAD edge after failed mutation"
    );
}

fn scenario_missing_value_rejects_insert(env: &FederationEnv, mutation_key: &str) {
    let err = gql_execute_idempotent_as_admin_expect_err(
        env,
        "INSERT (:City)-[:ROAD]->(:City)",
        mutation_key,
    );
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "expected missing inline value to fail with InvalidArgument, got {err:?}"
    );
}

fn scenario_null_value_rejects_insert(env: &FederationEnv, mutation_key: &str) {
    let err = gql_execute_idempotent_as_admin_expect_err(
        env,
        &format!("INSERT (:City)-[:ROAD {{{PROPERTY}: NULL}}]->(:City)"),
        mutation_key,
    );
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "expected NULL inline value to fail with InvalidArgument, got {err:?}"
    );
}

fn scenario_overflow_rejects_insert(env: &FederationEnv, mutation_key: &str) {
    let err = gql_execute_idempotent_as_admin_expect_err(
        env,
        &format!("INSERT (:City)-[:ROAD {{{PROPERTY}: 65536}}]->(:City)"),
        mutation_key,
    );
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "expected UINT16 overflow to fail with InvalidArgument, got {err:?}"
    );
}

fn scenario_insert_mixed_with_sidecar(env: &FederationEnv) {
    gql_execute_idempotent_as_admin(
        env,
        "INSERT (:City)-[:ROAD {distance: 7, surface: 'asphalt'}]->(:City)",
        "adr0034_inline_scalar_mutation_mixed_insert",
    );

    let result = gql_query_as_admin(
        env,
        "MATCH (a:City)-[e:ROAD]-(b:City) RETURN e.distance AS d, e.surface AS s",
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1, "expected exactly one mixed ROAD edge");
    assert_eq!(
        rows[0].get("d"),
        Some(&IcWireValue::Uint64(7)),
        "expected inline distance == 7"
    );
    assert_eq!(
        rows[0].get("s"),
        Some(&IcWireValue::Text("asphalt".into())),
        "expected sidecar surface == 'asphalt'"
    );
}

// ---------------------------------------------------------------------------
// Fixture family 1: full successful mutation lifecycle over a single edge.
// Former contracts preserved:
//   - inline_scalar_insert_round_trips_through_payload
//   - inline_scalar_set_updates_payload_value
//   - inline_scalar_remove_rejects
// ---------------------------------------------------------------------------

#[test]
fn inline_scalar_mutation_lifecycle() {
    let env = setup();

    scenario_insert_round_trips_through_payload(&env, 7, "adr0034_inline_scalar_mutation_insert");
    scenario_assert_distance(&env, 7);

    scenario_set_updates_payload_value(&env, 9, "adr0034_inline_scalar_mutation_set");
    scenario_assert_distance(&env, 9);

    scenario_remove_rejects_and_payload_unchanged(&env, 9, "adr0034_inline_scalar_mutation_remove");
}

// ---------------------------------------------------------------------------
// Fixture family 2: inline scalar coexists with an ordinary sidecar property.
// Former contract preserved:
//   - inline_scalar_insert_mixed_with_sidecar_property
// ---------------------------------------------------------------------------

#[test]
fn inline_scalar_mutation_mixed_persists_sidecar_and_payload() {
    let env = setup();
    scenario_insert_mixed_with_sidecar(&env);
}

// ---------------------------------------------------------------------------
// Fixture family 3: invalid-input rejection matrix on an otherwise empty graph.
// Each case uses a distinct mutation key and asserts no ROAD edge was created.
// Former contracts preserved:
//   - inline_scalar_missing_value_rejects_insert
//   - inline_scalar_null_value_rejects_insert
//   - inline_scalar_overflow_rejects_insert
// ---------------------------------------------------------------------------

#[test]
fn inline_scalar_mutation_rejection_matrix() {
    let env = setup();

    scenario_missing_value_rejects_insert(&env, "adr0034_inline_scalar_mutation_missing");
    scenario_assert_no_road_edge(&env);

    scenario_null_value_rejects_insert(&env, "adr0034_inline_scalar_mutation_null");
    scenario_assert_no_road_edge(&env);

    scenario_overflow_rejects_insert(&env, "adr0034_inline_scalar_mutation_overflow");
    scenario_assert_no_road_edge(&env);
}
