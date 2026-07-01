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

#[test]
fn inline_scalar_insert_round_trips_through_payload() {
    let env = setup();
    gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:City)-[:ROAD {distance: 7}]->(:City)",
        "adr0034_inline_scalar_mutation_insert",
    );

    let result = gql_query_as_admin(
        &env,
        "MATCH (a:City)-[e:ROAD]-(b:City) RETURN e.distance AS d",
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("d"), Some(&IcWireValue::Uint64(7)));
}

#[test]
fn inline_scalar_set_updates_payload_value() {
    let env = setup();
    gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:City)-[:ROAD {distance: 7}]->(:City)",
        "adr0034_inline_scalar_mutation_set_insert",
    );

    gql_execute_idempotent_as_admin(
        &env,
        "MATCH (a:City)-[e:ROAD]->(b:City) SET e.distance = 9 RETURN id(e) AS eid, e.distance AS d",
        "adr0034_inline_scalar_mutation_set",
    );

    let result = gql_query_as_admin(
        &env,
        "MATCH (a:City)-[e:ROAD]-(b:City) RETURN e.distance AS d",
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("d"), Some(&IcWireValue::Uint64(9)));
}

#[test]
fn inline_scalar_insert_mixed_with_sidecar_property() {
    let env = setup();
    gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:City)-[:ROAD {distance: 7, surface: 'asphalt'}]->(:City)",
        "adr0034_inline_scalar_mutation_mixed_insert",
    );

    let result = gql_query_as_admin(
        &env,
        "MATCH (a:City)-[e:ROAD]-(b:City) RETURN e.distance AS d, e.surface AS s",
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("d"), Some(&IcWireValue::Uint64(7)));
    assert_eq!(rows[0].get("s"), Some(&IcWireValue::Text("asphalt".into())));
}

#[test]
fn inline_scalar_missing_value_rejects_insert() {
    let env = setup();
    let err = gql_execute_idempotent_as_admin_expect_err(
        &env,
        "INSERT (:City)-[:ROAD]->(:City)",
        "adr0034_inline_scalar_mutation_missing",
    );
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "expected execution failure, got {err:?}"
    );

    // No edge should have been created.
    let result = gql_query_as_admin(&env, "MATCH (a:City)-[e:ROAD]-(b:City) RETURN e");
    assert_eq!(result.row_count, 0);
}

#[test]
fn inline_scalar_null_value_rejects_insert() {
    let env = setup();
    let err = gql_execute_idempotent_as_admin_expect_err(
        &env,
        "INSERT (:City)-[:ROAD {distance: NULL}]->(:City)",
        "adr0034_inline_scalar_mutation_null",
    );
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "expected execution failure, got {err:?}"
    );

    let result = gql_query_as_admin(&env, "MATCH (a:City)-[e:ROAD]-(b:City) RETURN e");
    assert_eq!(result.row_count, 0);
}

#[test]
fn inline_scalar_overflow_rejects_insert() {
    let env = setup();
    let err = gql_execute_idempotent_as_admin_expect_err(
        &env,
        "INSERT (:City)-[:ROAD {distance: 65536}]->(:City)",
        "adr0034_inline_scalar_mutation_overflow",
    );
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "expected execution failure, got {err:?}"
    );

    let result = gql_query_as_admin(&env, "MATCH (a:City)-[e:ROAD]-(b:City) RETURN e");
    assert_eq!(result.row_count, 0);
}

#[test]
fn inline_scalar_remove_rejects() {
    let env = setup();
    gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:City)-[:ROAD {distance: 7}]->(:City)",
        "adr0034_inline_scalar_mutation_remove_insert",
    );

    let err = gql_execute_idempotent_as_admin_expect_err(
        &env,
        "MATCH (a:City)-[e:ROAD]-(b:City) REMOVE e.distance",
        "adr0034_inline_scalar_mutation_remove",
    );
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "expected execution failure, got {err:?}"
    );

    // Payload unchanged.
    let result = gql_query_as_admin(
        &env,
        "MATCH (a:City)-[e:ROAD]-(b:City) RETURN e.distance AS d",
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("d"), Some(&IcWireValue::Uint64(7)));
}
