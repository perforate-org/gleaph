use gleaph_gql::parser;

use super::{PlanOp, build_block_plan};

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
