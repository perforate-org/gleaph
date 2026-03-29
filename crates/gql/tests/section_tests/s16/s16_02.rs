//! §16.2 — USE graph clause.
//!
//! GQL rule: `useGraphClause : USE graphExpression`

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── useGraphClause ──────────────────────────────────────────────────────
//   : USE graphExpression
//   ;
mod use_graph_clause {
    use super::*;

    /// USE myGraph MATCH (n) RETURN n — Focused { graph: "myGraph", body }
    #[test]
    fn use_graph_name() {
        let prog = p("USE myGraph MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let part = &cq.left.parts[0];
                match part {
                    SimpleQueryStatement::Focused { graph, .. } => {
                        assert_eq!(
                            graph.parts,
                            vec!["myGraph".to_string()],
                            "expected graph name 'myGraph'"
                        );
                    }
                    other => panic!("expected Focused, got {other:?}"),
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    /// USE myGraph { MATCH (n) RETURN n } — nested: InlineProcedureCall with use_graph
    #[test]
    fn use_graph_nested() {
        let prog = p("USE myGraph { MATCH (n) RETURN n }");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let part = &cq.left.parts[0];
                match part {
                    SimpleQueryStatement::InlineProcedureCall(ic) => {
                        let graph = ic
                            .use_graph
                            .as_ref()
                            .expect("expected use_graph to be Some");
                        assert_eq!(
                            graph.parts,
                            vec!["myGraph".to_string()],
                            "expected graph name 'myGraph'"
                        );
                    }
                    other => panic!("expected InlineProcedureCall, got {other:?}"),
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}
