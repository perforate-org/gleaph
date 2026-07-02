use gleaph_gql::parser;

use super::{PlanOp, build_block_plan, build_block_plan_with_schema_and_options};
use crate::path_extensions::{PlanBuildOptions, REJECTING_PATH_EXTENSION_HANDLER};
use crate::stats::TableStats;

fn parse_block(input: &str) -> gleaph_gql::ast::StatementBlock {
    let program = parser::parse(input).expect("query should parse");
    program
        .transaction_activity
        .expect("transaction activity")
        .body
        .expect("statement block")
}

#[test]
fn plans_delete_edge_when_binding_was_introduced_by_expand() {
    let block = parse_block("MATCH (a)-[e]->(b) DELETE e");
    let plan = build_block_plan(&block, None).expect("plan should build");
    assert!(
        plan.ops
            .iter()
            .any(|op| matches!(op, PlanOp::DeleteEdge { variable } if variable.as_ref() == "e"))
    );
}

#[test]
fn keeps_delete_vertex_for_node_binding() {
    let block = parse_block("MATCH (a:User) DELETE a");
    let plan = build_block_plan(&block, None).expect("plan should build");
    assert!(
        plan.ops
            .iter()
            .any(|op| matches!(op, PlanOp::DeleteVertex { variable } if variable.as_ref() == "a"))
    );
}

#[test]
fn block_plan_with_options_uses_stats_for_next_chain_cost_estimates() {
    let mut stats = TableStats::default();
    stats.label_cardinality.insert("A".to_string(), 100);
    stats.label_cardinality.insert("B".to_string(), 200);
    let block = parse_block("MATCH (a:A) RETURN a NEXT MATCH (b:B) RETURN b");

    let with_stats = build_block_plan_with_schema_and_options(
        &block,
        &gleaph_gql::type_check::NoSchema,
        PlanBuildOptions {
            stats: Some(&stats),
            path_extensions: &REJECTING_PATH_EXTENSION_HANDLER,
        },
    )
    .expect("plan should build");

    let no_stats = build_block_plan_with_schema_and_options(
        &block,
        &gleaph_gql::type_check::NoSchema,
        PlanBuildOptions {
            stats: None,
            path_extensions: &REJECTING_PATH_EXTENSION_HANDLER,
        },
    )
    .expect("plan should build without stats");

    // If options.stats is ignored, the two plans would have identical estimates.
    assert_ne!(
        with_stats.annotations.optimizer.estimated_cost,
        no_stats.annotations.optimizer.estimated_cost,
        "NEXT-chain cost estimate must differ when label cardinalities are supplied"
    );
    assert_ne!(
        with_stats.annotations.optimizer.estimated_rows,
        no_stats.annotations.optimizer.estimated_rows,
        "NEXT-chain row estimate must differ when label cardinalities are supplied"
    );

    // Concrete sanity checks against the supplied cardinalities.
    assert_eq!(
        with_stats.annotations.optimizer.estimated_rows,
        Some(200.0),
        "final row estimate should come from the second scan label B (cardinality 200)"
    );
}
