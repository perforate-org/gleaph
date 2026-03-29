//! §11 — Graph and binding table expressions.
//!
//! GQL rules: graphExpression, currentGraph, bindingTableExpression.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── graphExpression ─────────────────────────────────────────────────────
//   : graphReference
//   | objectExpressionPrimary
//   | objectNameOrBindingVariable
//   | currentGraph
//   ;
mod graph_expression {
    use super::*;

    /// objectNameOrBindingVariable — USE <name> scopes the query
    #[test]
    fn object_name() {
        let prog = p("USE myGraph MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(q) => {
                assert!(!q.left.parts.is_empty());
                assert!(matches!(
                    &q.left.parts[0],
                    SimpleQueryStatement::Focused { graph, .. }
                        if graph.parts == ["myGraph"]
                ));
            }
            other => panic!("expected Statement::Query, got {other:?}"),
        }
    }

    /// graphReference — catalog-qualified graph name
    #[test]
    fn catalog_path() {
        let prog = p("USE /db/myGraph MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(q) => {
                assert!(matches!(
                    &q.left.parts[0],
                    SimpleQueryStatement::Focused { graph, .. }
                        if graph.parts.len() == 2
                ));
            }
            other => panic!("expected Statement::Query, got {other:?}"),
        }
    }
}

// ── currentGraph ────────────────────────────────────────────────────────
//   : CURRENT_PROPERTY_GRAPH
//   | CURRENT_GRAPH
//   ;
mod current_graph {
    use super::*;

    /// CURRENT_GRAPH in USE clause
    #[test]
    fn current_graph_in_use() {
        let prog = p("USE CURRENT_GRAPH MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(q) => {
                assert!(matches!(
                    &q.left.parts[0],
                    SimpleQueryStatement::Focused { graph, .. }
                        if graph.parts == ["CURRENT_GRAPH"]
                ));
            }
            other => panic!("expected Statement::Query, got {other:?}"),
        }
    }

    /// CURRENT_PROPERTY_GRAPH in USE clause
    #[test]
    fn current_property_graph_in_use() {
        let prog = p("USE CURRENT_PROPERTY_GRAPH MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(q) => {
                assert!(matches!(
                    &q.left.parts[0],
                    SimpleQueryStatement::Focused { graph, .. }
                        if graph.parts == ["CURRENT_PROPERTY_GRAPH"]
                ));
            }
            other => panic!("expected Statement::Query, got {other:?}"),
        }
    }

    /// HOME_GRAPH in USE clause
    #[test]
    fn home_graph_in_use() {
        let prog = p("USE HOME_GRAPH MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(q) => {
                assert!(matches!(
                    &q.left.parts[0],
                    SimpleQueryStatement::Focused { graph, .. }
                        if graph.parts == ["HOME_GRAPH"]
                ));
            }
            other => panic!("expected Statement::Query, got {other:?}"),
        }
    }

    /// HOME_PROPERTY_GRAPH in USE clause
    #[test]
    fn home_property_graph_in_use() {
        let prog = p("USE HOME_PROPERTY_GRAPH MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(q) => {
                assert!(matches!(
                    &q.left.parts[0],
                    SimpleQueryStatement::Focused { graph, .. }
                        if graph.parts == ["HOME_PROPERTY_GRAPH"]
                ));
            }
            other => panic!("expected Statement::Query, got {other:?}"),
        }
    }

    /// CURRENT_GRAPH in SESSION SET GRAPH
    #[test]
    fn current_graph_in_session_set() {
        let prog = p("SESSION SET GRAPH CURRENT_GRAPH");
        assert_eq!(prog.session_activity.len(), 1);
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::Graph { name, .. }) => {
                assert_eq!(name.parts, ["CURRENT_GRAPH"]);
            }
            other => panic!("expected SessionSetCommand::Graph, got {other:?}"),
        }
    }

    /// HOME_GRAPH in SESSION SET GRAPH
    #[test]
    fn home_graph_in_session_set() {
        let prog = p("SESSION SET GRAPH HOME_GRAPH");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::Graph { name, .. }) => {
                assert_eq!(name.parts, ["HOME_GRAPH"]);
            }
            other => panic!("expected SessionSetCommand::Graph, got {other:?}"),
        }
    }
}

// ── bindingTableExpression ──────────────────────────────────────────────
//   : nestedBindingTableQuerySpecification
//   | bindingTableReference
//   | objectExpressionPrimary
//   | objectNameOrBindingVariable
//   ;
mod binding_table_expression {
    use super::*;

    /// objectNameOrBindingVariable — TABLE t = <name>
    #[test]
    fn object_name() {
        let prog = p("TABLE t = myTable MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(q) => {
                let binding = &q.left.prefix_bindings[0];
                assert_eq!(binding.kind, ProcedureBindingKind::Table);
                assert!(matches!(
                    &binding.initializer,
                    ProcedureBindingInitializer::Object(name) if name.parts == ["myTable"]
                ));
            }
            other => panic!("expected Statement::Query, got {other:?}"),
        }
    }

    /// nestedBindingTableQuerySpecification — TABLE t = { ... }
    #[test]
    fn nested_query() {
        let prog = p("TABLE t = { MATCH (n) RETURN n } MATCH (m) RETURN m");
        let b = body(&prog);
        match &b.first {
            Statement::Query(q) => {
                let binding = &q.left.prefix_bindings[0];
                assert_eq!(binding.kind, ProcedureBindingKind::Table);
                assert!(matches!(
                    &binding.initializer,
                    ProcedureBindingInitializer::Query(_)
                ));
            }
            other => panic!("expected Statement::Query, got {other:?}"),
        }
    }

    /// bindingTableReference — catalog-qualified name
    #[test]
    fn catalog_path() {
        let prog = p("TABLE t = /db/myTable MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(q) => {
                let binding = &q.left.prefix_bindings[0];
                assert!(matches!(
                    &binding.initializer,
                    ProcedureBindingInitializer::Object(name) if name.parts.len() == 2
                ));
            }
            other => panic!("expected Statement::Query, got {other:?}"),
        }
    }
}

// ── focused query scoping ───────────────────────────────────────────────
mod focused_query {
    use super::*;

    /// USE with body is None when it scopes a RETURN
    #[test]
    fn use_graph_return_only() {
        let prog = p("USE myGraph RETURN 1");
        let b = body(&prog);
        match &b.first {
            Statement::Query(q) => {
                assert!(matches!(
                    &q.left.parts[0],
                    SimpleQueryStatement::Focused { graph, body: None }
                        if graph.parts == ["myGraph"]
                ));
            }
            other => panic!("expected Statement::Query, got {other:?}"),
        }
    }

    /// USE with body wrapping a MATCH
    #[test]
    fn use_graph_match() {
        let prog = p("USE myGraph MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(q) => match &q.left.parts[0] {
                SimpleQueryStatement::Focused { graph, body } => {
                    assert_eq!(graph.parts, ["myGraph"]);
                    assert!(body.is_some());
                }
                other => panic!("expected Focused, got {other:?}"),
            },
            other => panic!("expected Statement::Query, got {other:?}"),
        }
    }
}
