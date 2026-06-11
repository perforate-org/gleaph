//! PocketIC: federated graph shard executes router `seed_bindings_blob` on the wire path.

use candid::Encode;
use gleaph_gql::Value;
use gleaph_gql::ast::{CmpOp, Expr};
use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp, ProjectColumn, ScanValue};
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanArgs, ExecutePlanResult, GqlExecutionMode, SeedBindingEntry, SeedBindingsWire,
};
use gleaph_pocket_ic_tests::{SOURCE_SHARD, e2e_insert_vertex, install_federation, query_as_router};
use std::rc::Rc;

#[test]
fn graph_execute_plan_query_skips_index_scan_with_seed_bindings() {
    let env = install_federation();
    let inserted = e2e_insert_vertex(&env, env.graph_source);

    let plan = PhysicalPlan::from_ops(vec![
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
    ]);
    let plan_blob = encode_block_plans(std::slice::from_ref(&plan), false).expect("encode plan");
    let seeds = SeedBindingsWire {
        entries: vec![SeedBindingEntry {
            variable: "n".into(),
            local_vertex_ids: vec![inserted.local_vertex_id],
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
        },
    );

    assert_eq!(result.row_count, 1);
    assert!(result.rows_blob.is_some());
}
