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

#[test]
fn next_insert_edge_reuses_matched_vertices() {
    let block = parse_block(
        "MATCH (a:BindNextUser {id: 'alice'}), (b:BindNextUser {id: 'bob'}) RETURN a NEXT INSERT (a)-[:BIND_NEXT_FOLLOWS]->(b)",
    );
    let plan = build_block_plan(&block, None).expect("plan should build");

    // The edge must reference the matched vertices directly.
    assert!(
        plan.ops.iter().any(|op| matches!(
            op,
            PlanOp::InsertEdge { src, dst, .. } if src.as_ref() == "a" && dst.as_ref() == "b"
        )),
        "plan must insert an edge between the matched vertices"
    );

    // Neither endpoint should be recreated as a new vertex.
    assert!(
        !plan.ops.iter().any(|op| matches!(
            op,
            PlanOp::InsertVertex { variable: Some(v), .. } if v.as_ref() == "a" || v.as_ref() == "b"
        )),
        "matched vertex endpoints must not be re-inserted"
    );

    // The boundary projection must retain the non-returned destination binding.
    let project = plan
        .ops
        .iter()
        .find_map(|op| match op {
            PlanOp::Project { columns, .. } => Some(columns),
            _ => None,
        })
        .expect("prior statement should end with a Project");
    assert!(
        project.iter().any(|col| matches!(
            &col.expr.kind,
            gleaph_gql::ast::ExprKind::Variable(v) if v.as_str() == "b"
        )),
        "boundary projection must preserve the hidden vertex binding for b"
    );
}

#[test]
fn next_call_before_insert_reuses_matched_vertex() {
    let block = parse_block(
        "MATCH (a:BindNextUser {id: 'alice'}) RETURN a NEXT CALL GLEAPH.FINALIZE_FORWARD_EDGE_SPAN(a) RETURN a NEXT INSERT (a)-[:BIND_NEXT_FOLLOWS]->(a)",
    );
    let plan = build_block_plan(&block, None).expect("plan should build");

    let call_index = plan
        .ops
        .iter()
        .position(|op| matches!(
            op,
            PlanOp::CallProcedure { name, args, .. }
                if name.iter().map(|part| part.as_ref()).eq(["GLEAPH", "FINALIZE_FORWARD_EDGE_SPAN"])
                    && matches!(args.as_slice(), [gleaph_gql::ast::Expr { kind: gleaph_gql::ast::ExprKind::Variable(variable), .. }] if variable == "a")
        ))
        .expect("plan must finalize the matched vertex");
    let insert_index = plan
        .ops
        .iter()
        .position(|op| matches!(op, PlanOp::InsertEdge { src, .. } if src.as_ref() == "a"))
        .expect("plan must insert the edge");
    assert!(
        call_index < insert_index,
        "finalize must precede the edge insert"
    );
}

#[test]
fn next_insert_edge_without_yield_preserves_all_typed_bindings() {
    // Two non-returned matched vertices must both survive as typed bindings so two
    // separate NEXT INSERT edges can reuse them against a shared source.
    let block = parse_block(
        "MATCH (a:BindNextUser {id: 'alice'}), (b:BindNextUser {id: 'bob'}), (c:BindNextUser {id: 'carol'}) RETURN a NEXT INSERT (a)-[:BIND_NEXT_FOLLOWS]->(b)",
    );
    let plan = build_block_plan(&block, None).expect("plan should build");

    let project = plan
        .ops
        .iter()
        .find_map(|op| match op {
            PlanOp::Project { columns, .. } => Some(columns),
            _ => None,
        })
        .expect("prior statement should end with a Project");
    let hidden_names: std::collections::BTreeSet<&str> = project
        .iter()
        .filter_map(|col| match &col.expr.kind {
            gleaph_gql::ast::ExprKind::Variable(v) => Some(v.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        hidden_names.contains("a") && hidden_names.contains("b") && hidden_names.contains("c"),
        "boundary projection must keep all matched typed bindings, not only RETURN items"
    );
}
