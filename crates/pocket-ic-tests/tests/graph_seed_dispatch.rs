//! PocketIC: federated graph shard executes router `seed_bindings_blob` on the wire path.

use candid::{Decode, Encode};
use gleaph_gql::Value;
use gleaph_gql::ast::{CmpOp, Expr};
use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp, ProjectColumn, ScanValue};
use gleaph_gql_planner::wire::{decode_plan_bundle, encode_block_plans};
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanArgs, ExecutePlanResult, GqlExecutionMode, SeedBindingEntry, SeedBindingsWire,
};
use gleaph_pocket_ic_tests::{
    SOURCE_SHARD, e2e_insert_vertex, execute_plan_query_as_router_reject, install_federation,
    query_as_router,
};
use std::rc::Rc;

fn index_scan_project_plan() -> PhysicalPlan {
    PhysicalPlan::from_ops(vec![
        PlanOp::IndexScan {
            variable: Rc::from("n"),
            property: Rc::from("age"),
            value: ScanValue::Literal(Value::Int64(5)),
            cmp: CmpOp::Eq,
            property_projection: None,
        },
        PlanOp::Project {
            columns: vec![ProjectColumn {
                expr: Expr::var("n"),
                alias: Some(Rc::from("n")),
            }],
            distinct: false,
        },
    ])
}

#[test]
fn graph_execute_plan_query_skips_index_scan_with_seed_bindings() {
    let env = install_federation();
    let inserted = e2e_insert_vertex(&env, env.graph_source);

    let plan = index_scan_project_plan();
    let plan_blob = encode_block_plans(std::slice::from_ref(&plan), false).expect("encode plan");
    let seeds = SeedBindingsWire {
        entries: vec![SeedBindingEntry {
            variable: "n".into(),
            local_vertex_ids: vec![inserted.local_vertex_id],
            local_edge_postings: Vec::new(),
        }],
    };
    let seed_blob = Encode!(&seeds).expect("encode seeds");

    let result: ExecutePlanResult = query_as_router(
        &env,
        env.graph_source,
        "execute_plan_query",
        ExecutePlanArgs {
            target_shard_id: SOURCE_SHARD,
            mutation_id: None,
            plan_blob,
            params_blob: vec![],
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: Some(seed_blob),
            resolved_labels: None,
            resolved_properties: None,
        },
    );

    assert_eq!(result.row_count, 1);
    assert!(result.rows_blob.is_some());
}

#[test]
fn execute_plan_args_without_seeds_preserves_plan_blob_roundtrip() {
    let plan = index_scan_project_plan();
    let plan_blob = encode_block_plans(std::slice::from_ref(&plan), false).expect("encode plan");
    let args = ExecutePlanArgs {
        target_shard_id: SOURCE_SHARD,
        mutation_id: None,
        plan_blob: plan_blob.clone(),
        params_blob: vec![],
        mode: GqlExecutionMode::Query,
        seed_bindings_blob: None,
        resolved_labels: None,
        resolved_properties: None,
    };
    let bytes = Encode!(&args).expect("encode args");
    let decoded: ExecutePlanArgs = Decode!(&bytes, ExecutePlanArgs).expect("decode args");
    assert_eq!(decoded.plan_blob, plan_blob);
    decode_plan_bundle(&decoded.plan_blob).expect("decode plan bundle");
}

#[test]
fn graph_execute_plan_query_rejects_index_scan_without_seeds() {
    let env = install_federation();
    let _ = e2e_insert_vertex(&env, env.graph_source);
    let plan = index_scan_project_plan();
    let plan_blob = encode_block_plans(std::slice::from_ref(&plan), false).expect("encode plan");
    decode_plan_bundle(&plan_blob).expect("host decode before ic call");
    let err = execute_plan_query_as_router_reject(
        &env,
        env.graph_source,
        ExecutePlanArgs {
            target_shard_id: SOURCE_SHARD,
            mutation_id: None,
            plan_blob,
            params_blob: vec![],
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: None,
            resolved_labels: None,
            resolved_properties: None,
        },
    );

    assert!(
        err.contains("IndexScan(no index client)"),
        "unexpected error: {err}"
    );
}
