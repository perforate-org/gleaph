//! `GPL` plan bundle (version byte `1`) roundtrip for representative queries.

use gleaph_gql::parser;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_planner::NoStats;
use gleaph_gql_planner::wire::{PLAN_WIRE_MAGIC, PLAN_WIRE_VERSION, encode_block_plans};
use gleaph_gql_planner::{build_block_plan_with_schema, wire::decode_plan_bundle};

#[test]
fn index_scan_plan_roundtrips() {
    let gql = "MATCH (n:User) WHERE n.id = $id RETURN n";
    let program = parser::parse(gql).expect("parse");
    let block = program
        .transaction_activity
        .expect("tx")
        .body
        .expect("body");
    let plan = build_block_plan_with_schema(&block, Some(&NoStats), &NoSchema).expect("plan");
    let blob = encode_block_plans(std::slice::from_ref(&plan), false).expect("encode");
    assert_eq!(&blob[0..3], &PLAN_WIRE_MAGIC);
    assert_eq!(blob[3], PLAN_WIRE_VERSION);
    let (write, decoded) = decode_plan_bundle(&blob).expect("decode");
    assert!(!write);
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].ops.len(), plan.ops.len());
}
