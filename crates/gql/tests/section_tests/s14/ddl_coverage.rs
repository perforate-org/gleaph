//! §14 — DDL coverage (CREATE GRAPH, schema statements).

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::EdgeDirection;

// ── DDL coverage (parser/ddl.rs) ────────────────────────────────────────

/// Helper: extract the first statement from body.
fn first_stmt(prog: &GqlProgram) -> &Statement {
    &body(prog).first
}

#[test]
fn drop_schema() {
    let prog = p("DROP SCHEMA /mySchema");
    assert!(matches!(first_stmt(&prog), Statement::DropSchema(_)));
}

#[test]
fn drop_schema_if_exists() {
    let prog = p("DROP SCHEMA IF EXISTS /mySchema");
    match first_stmt(&prog) {
        Statement::DropSchema(ds) => assert!(ds.if_exists),
        other => panic!("expected DropSchema, got {other:?}"),
    }
}

#[test]
fn drop_graph() {
    let prog = p("DROP GRAPH myGraph");
    assert!(matches!(first_stmt(&prog), Statement::DropGraph(_)));
}

#[test]
fn drop_graph_if_exists() {
    let prog = p("DROP GRAPH IF EXISTS myGraph");
    match first_stmt(&prog) {
        Statement::DropGraph(dg) => assert!(dg.if_exists),
        other => panic!("expected DropGraph, got {other:?}"),
    }
}

#[test]
fn drop_graph_type() {
    let prog = p("DROP GRAPH TYPE myType");
    assert!(matches!(first_stmt(&prog), Statement::DropGraphType(_)));
}

#[test]
fn create_graph_like() {
    let prog = p("CREATE GRAPH myGraph LIKE otherGraph");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            assert!(matches!(cg.graph_type, Some(GraphTypeSpec::Like(_))));
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_typed() {
    let prog = p("CREATE GRAPH myGraph TYPED myType");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            assert!(matches!(
                cg.graph_type,
                Some(GraphTypeSpec::Typed {
                    typed_keyword: true,
                    ..
                })
            ));
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_typed_double_colon() {
    let prog = p("CREATE GRAPH myGraph :: myType");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            assert!(matches!(
                cg.graph_type,
                Some(GraphTypeSpec::Typed {
                    typed_keyword: false,
                    ..
                })
            ));
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_schema() {
    let prog = p("CREATE SCHEMA /mydb");
    assert!(matches!(first_stmt(&prog), Statement::CreateSchema(_)));
}

#[test]
fn create_schema_if_not_exists() {
    let prog = p("CREATE SCHEMA IF NOT EXISTS /mydb");
    match first_stmt(&prog) {
        Statement::CreateSchema(cs) => assert!(cs.if_not_exists),
        other => panic!("expected CreateSchema, got {other:?}"),
    }
}

#[test]
fn create_graph_type_empty() {
    let prog = p("CREATE GRAPH TYPE myType {}");
    assert!(matches!(first_stmt(&prog), Statement::CreateGraphType(_)));
}

#[test]
fn create_graph_if_not_exists() {
    let prog = p("CREATE GRAPH IF NOT EXISTS myGraph {}");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => assert!(cg.if_not_exists),
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_or_replace_graph() {
    let prog = p("CREATE OR REPLACE GRAPH myGraph {}");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => assert!(cg.or_replace),
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_any() {
    let prog = p("CREATE GRAPH myGraph ANY");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            assert!(matches!(cg.graph_type, Some(GraphTypeSpec::Any { .. })));
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_copy_of() {
    let prog = p("CREATE GRAPH myGraph {} AS COPY OF otherGraph");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => assert!(cg.copy_of.is_some()),
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_type_copy_of() {
    let prog = p("CREATE GRAPH TYPE myType COPY OF otherType {}");
    match first_stmt(&prog) {
        Statement::CreateGraphType(cgt) => assert!(cgt.copy_of.is_some()),
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

#[test]
fn create_graph_with_node_and_edge_type() {
    let prog = p(
        "CREATE GRAPH myGraph { NODE Person LABEL Person { name STRING }, DIRECTED EDGE Knows LABEL Knows CONNECTING (Person -> Person) }",
    );
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            let def = cg.graph_type.as_ref().unwrap();
            if let GraphTypeSpec::Inline(d) = def {
                assert_eq!(d.elements.len(), 2);
            } else {
                panic!("expected Inline, got {def:?}");
            }
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_type_if_not_exists() {
    let prog = p("CREATE GRAPH TYPE IF NOT EXISTS myType {}");
    match first_stmt(&prog) {
        Statement::CreateGraphType(cgt) => assert!(cgt.if_not_exists),
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

// ── pattern-style graph type elements ─────────────────────────────

#[test]
fn create_graph_pattern_node_no_name() {
    let prog = p("CREATE GRAPH myGraph {(:Person)}");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            if let Some(GraphTypeSpec::Inline(d)) = &cg.graph_type {
                assert_eq!(d.elements.len(), 1);
                assert!(matches!(&d.elements[0], GraphTypeElement::Node(_)));
            } else {
                panic!("expected Inline graph type");
            }
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_pattern_edge_left_arrow() {
    let prog = p("CREATE GRAPH myGraph {(A :A), (B :B), (A)<-[R :R]-(B)}");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            if let Some(GraphTypeSpec::Inline(d)) = &cg.graph_type {
                assert_eq!(d.elements.len(), 3);
                if let GraphTypeElement::Edge(e) = &d.elements[2] {
                    assert_eq!(e.direction, EdgeDirection::PointingLeft);
                } else {
                    panic!("expected Edge element");
                }
            } else {
                panic!("expected Inline graph type");
            }
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_pattern_edge_undirected() {
    let prog = p("CREATE GRAPH myGraph {(A :A), (B :B), (A)~[R :R]~(B)}");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            if let Some(GraphTypeSpec::Inline(d)) = &cg.graph_type {
                if let GraphTypeElement::Edge(e) = &d.elements[2] {
                    assert_eq!(e.direction, EdgeDirection::Undirected);
                } else {
                    panic!("expected Edge element");
                }
            } else {
                panic!("expected Inline graph type");
            }
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_pattern_edge_no_label() {
    let prog = p("CREATE GRAPH myGraph {(A :A), (B :B), (A)-[R]->(B)}");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            if let Some(GraphTypeSpec::Inline(d)) = &cg.graph_type {
                if let GraphTypeElement::Edge(e) = &d.elements[2] {
                    assert!(e.label_set.is_none());
                } else {
                    panic!("expected Edge element");
                }
            } else {
                panic!("expected Inline graph type");
            }
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

// ── phrase-style graph type elements ──────────────────────────────

#[test]
fn create_graph_vertex_type() {
    let prog = p("CREATE GRAPH myGraph { VERTEX Person LABEL Person { name STRING } }");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            if let Some(GraphTypeSpec::Inline(d)) = &cg.graph_type {
                if let GraphTypeElement::Node(n) = &d.elements[0] {
                    assert_eq!(n.keyword.0, "VERTEX");
                } else {
                    panic!("expected Node element");
                }
            } else {
                panic!("expected Inline graph type");
            }
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_node_type_no_label() {
    let prog = p("CREATE GRAPH myGraph { NODE Person { name STRING } }");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            if let Some(GraphTypeSpec::Inline(d)) = &cg.graph_type {
                if let GraphTypeElement::Node(n) = &d.elements[0] {
                    assert!(n.label_set.is_none());
                } else {
                    panic!("expected Node element");
                }
            } else {
                panic!("expected Inline graph type");
            }
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_undirected_edge() {
    let prog = p("CREATE GRAPH myGraph { UNDIRECTED EDGE Knows CONNECTING (A ~ B) }");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            if let Some(GraphTypeSpec::Inline(d)) = &cg.graph_type {
                if let GraphTypeElement::Edge(e) = &d.elements[0] {
                    assert_eq!(e.direction, EdgeDirection::Undirected);
                    assert_eq!(e.keyword.0, "EDGE");
                } else {
                    panic!("expected Edge element");
                }
            } else {
                panic!("expected Inline graph type");
            }
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_undirected_relationship() {
    let prog = p("CREATE GRAPH myGraph { UNDIRECTED RELATIONSHIP Knows CONNECTING (A ~ B) }");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            if let Some(GraphTypeSpec::Inline(d)) = &cg.graph_type {
                if let GraphTypeElement::Edge(e) = &d.elements[0] {
                    assert_eq!(e.keyword.0, "RELATIONSHIP");
                } else {
                    panic!("expected Edge element");
                }
            } else {
                panic!("expected Inline graph type");
            }
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_edge_no_label() {
    let prog = p("CREATE GRAPH myGraph { DIRECTED EDGE Knows CONNECTING (A -> B) }");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            if let Some(GraphTypeSpec::Inline(d)) = &cg.graph_type {
                if let GraphTypeElement::Edge(e) = &d.elements[0] {
                    assert!(e.label_set.is_none());
                } else {
                    panic!("expected Edge element");
                }
            } else {
                panic!("expected Inline graph type");
            }
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_edge_left_endpoint() {
    let prog = p("CREATE GRAPH myGraph { DIRECTED EDGE Knows CONNECTING (A <- B) }");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            if let Some(GraphTypeSpec::Inline(d)) = &cg.graph_type {
                if let GraphTypeElement::Edge(e) = &d.elements[0] {
                    // A <- B means source=B, destination=A
                    assert_eq!(e.source.type_name.as_deref(), Some("B"));
                    assert_eq!(e.destination.type_name.as_deref(), Some("A"));
                } else {
                    panic!("expected Edge element");
                }
            } else {
                panic!("expected Inline graph type");
            }
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

// ── graph type spec variants ─────────────────────────────────────

#[test]
fn create_graph_typed_inline() {
    let prog = p("CREATE GRAPH myGraph TYPED {(Person :Person)}");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            assert!(matches!(cg.graph_type, Some(GraphTypeSpec::Inline(_))));
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_inline_braces() {
    let prog = p("CREATE GRAPH myGraph {(Person :Person)}");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            assert!(matches!(cg.graph_type, Some(GraphTypeSpec::Inline(_))));
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_type_as_copy_of() {
    let prog = p("CREATE GRAPH TYPE myType AS COPY OF otherType {}");
    match first_stmt(&prog) {
        Statement::CreateGraphType(cgt) => {
            assert!(cgt.as_keyword);
            assert!(cgt.copy_of.is_some());
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

#[test]
fn create_graph_type_no_brace_body() {
    let prog = p("CREATE GRAPH myGraph ANY");
    match first_stmt(&prog) {
        Statement::CreateGraph(cg) => {
            assert!(matches!(cg.graph_type, Some(GraphTypeSpec::Any { .. })));
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}
