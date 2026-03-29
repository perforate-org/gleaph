//! Additional clause/DDL/validate coverage tests.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql::validate::validate;

// ── Clause coverage (parser/clause.rs) ──────────────────────────────────

mod clause_coverage {
    use super::*;

    fn return_body(prog: &GqlProgram) -> &ReturnBody {
        let b = body(prog);
        match &b.first {
            Statement::Query(cq) => match cq.left.result.as_ref().unwrap() {
                ResultStatement::Return(ret) => &ret.body,
                other => panic!("expected Return, got {other:?}"),
            },
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn ascending_direction() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.name ASCENDING");
        match return_body(&prog) {
            ReturnBody::Items { order_by, .. } => {
                let order = order_by.as_ref().unwrap();
                assert_eq!(order.items[0].direction, Some(SortDirection::Ascending));
            }
            other => panic!("expected Items, got {other:?}"),
        }
    }

    #[test]
    fn descending_direction() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.name DESCENDING");
        match return_body(&prog) {
            ReturnBody::Items { order_by, .. } => {
                let order = order_by.as_ref().unwrap();
                assert_eq!(order.items[0].direction, Some(SortDirection::Descending));
            }
            other => panic!("expected Items, got {other:?}"),
        }
    }

    #[test]
    fn nulls_first() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.name NULLS FIRST");
        match return_body(&prog) {
            ReturnBody::Items { order_by, .. } => {
                let order = order_by.as_ref().unwrap();
                assert_eq!(order.items[0].null_order, Some(NullOrder::First));
            }
            other => panic!("expected Items, got {other:?}"),
        }
    }

    #[test]
    fn nulls_last() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.name NULLS LAST");
        match return_body(&prog) {
            ReturnBody::Items { order_by, .. } => {
                let order = order_by.as_ref().unwrap();
                assert_eq!(order.items[0].null_order, Some(NullOrder::Last));
            }
            other => panic!("expected Items, got {other:?}"),
        }
    }

    #[test]
    fn empty_group_by() {
        let prog = p("MATCH (n) RETURN COUNT(*) GROUP BY ()");
        match return_body(&prog) {
            ReturnBody::Items { group_by, .. } => {
                let group = group_by.as_ref().unwrap();
                assert!(group.items.is_empty());
            }
            other => panic!("expected Items, got {other:?}"),
        }
    }
}

// ── DDL coverage (parser/ddl.rs) ────────────────────────────────────────

mod ddl_coverage {
    use super::*;

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
}

// ── Validate.rs coverage ────────────────────────────────────────────────

mod validate_coverage {
    use super::*;

    #[test]
    fn contradictory_access_modes() {
        let result =
            gleaph_gql::parser::parse("START TRANSACTION READ ONLY, READ WRITE MATCH (n) RETURN n");
        if let Ok(prog) = result {
            let err = validate(&prog);
            assert!(err.is_err());
        }
    }

    #[test]
    fn group_by_with_binary_expr() {
        let prog = p("MATCH (n) RETURN n.x + n.y, COUNT(*) GROUP BY n.x + n.y");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_unary_expr() {
        let prog = p("MATCH (n) RETURN -n.x, COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_function() {
        let prog = p("MATCH (n) RETURN UPPER(n.name), COUNT(*) GROUP BY n.name");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_is_null() {
        let prog = p("MATCH (n) RETURN n.x IS NULL, COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_case() {
        let prog = p(
            "MATCH (n) RETURN CASE WHEN n.x > 0 THEN 'pos' ELSE 'neg' END, COUNT(*) GROUP BY n.x",
        );
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_cast() {
        let prog = p("MATCH (n) RETURN CAST(n.x AS STRING), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_coalesce() {
        let prog = p("MATCH (n) RETURN COALESCE(n.x, 0), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_not_compatible_error() {
        let prog = p("MATCH (n) RETURN n.x, n.y, COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_err());
    }

    #[test]
    fn composite_query_mismatched_bindings() {
        let result =
            gleaph_gql::parser::parse("MATCH (n) RETURN n.x UNION ALL MATCH (m) RETURN m.x AS y");
        if let Ok(prog) = result {
            assert!(validate(&prog).is_err());
        }
    }

    #[test]
    fn set_statement_validation() {
        let prog = p("MATCH (n) SET n.x = 42");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn remove_statement_validation() {
        let prog = p("MATCH (n) REMOVE n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn delete_statement_validation() {
        let prog = p("MATCH (n) DELETE n");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_concat() {
        let prog = p("MATCH (n) RETURN n.first || n.last, COUNT(*) GROUP BY n.first, n.last");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_normalize() {
        let prog = p("MATCH (n) RETURN NORMALIZE(n.name), COUNT(*) GROUP BY n.name");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_abs() {
        let prog = p("MATCH (n) RETURN ABS(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_property_access() {
        let prog = p("MATCH (n) RETURN n.x, COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_list_literal() {
        let prog = p("MATCH (n) RETURN [n.x, n.y], COUNT(*) GROUP BY n.x, n.y");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_paren() {
        let prog = p("MATCH (n) RETURN (n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn insert_validation() {
        let prog = p("INSERT (:Person {name: 'Alice'})");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_char_length() {
        let prog = p("MATCH (n) RETURN CHAR_LENGTH(n.name), COUNT(*) GROUP BY n.name");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_nullif() {
        let prog = p("MATCH (n) RETURN NULLIF(n.x, 0), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn select_without_match_validation() {
        let prog = p("SELECT n.name FROM myGraph MATCH (n)");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    // ── Group compatibility for various expression types ─────────────

    #[test]
    fn group_by_with_not() {
        let prog = p("MATCH (n) RETURN NOT n.active, COUNT(*) GROUP BY n.active");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_floor() {
        let prog = p("MATCH (n) RETURN FLOOR(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_ceil() {
        let prog = p("MATCH (n) RETURN CEIL(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_sqrt() {
        let prog = p("MATCH (n) RETURN SQRT(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_exp() {
        let prog = p("MATCH (n) RETURN EXP(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_ln() {
        let prog = p("MATCH (n) RETURN LN(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_log10() {
        let prog = p("MATCH (n) RETURN LOG10(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_sin() {
        let prog = p("MATCH (n) RETURN SIN(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_cos() {
        let prog = p("MATCH (n) RETURN COS(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_tan() {
        let prog = p("MATCH (n) RETURN TAN(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_asin() {
        let prog = p("MATCH (n) RETURN ASIN(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_acos() {
        let prog = p("MATCH (n) RETURN ACOS(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_atan() {
        let prog = p("MATCH (n) RETURN ATAN(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_lower() {
        let prog = p("MATCH (n) RETURN LOWER(n.name), COUNT(*) GROUP BY n.name");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_byte_length() {
        let prog = p("MATCH (n) RETURN BYTE_LENGTH(n.name), COUNT(*) GROUP BY n.name");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_element_id() {
        let prog = p("MATCH (n) RETURN ELEMENT_ID(n), COUNT(*) GROUP BY n");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_is_labeled() {
        let prog = p("MATCH (n) RETURN n :Person, COUNT(*) GROUP BY n");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_trim() {
        let prog = p("MATCH (n) RETURN TRIM(' ' FROM n.name), COUNT(*) GROUP BY n.name");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_btrim() {
        let prog = p("MATCH (n) RETURN BTRIM(n.name, ' '), COUNT(*) GROUP BY n.name");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_left() {
        let prog = p("MATCH (n) RETURN LEFT(n.name, 3), COUNT(*) GROUP BY n.name");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_right() {
        let prog = p("MATCH (n) RETURN RIGHT(n.name, 3), COUNT(*) GROUP BY n.name");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_power() {
        let prog = p("MATCH (n) RETURN POWER(n.x, 2), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_mod() {
        let prog = p("MATCH (n) RETURN MOD(n.x, 3), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[cfg(feature = "cypher")]
    #[test]
    fn group_by_with_list_index() {
        let prog = p("MATCH (n) RETURN n.items[0], COUNT(*) GROUP BY n.items");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[cfg(feature = "cypher")]
    #[test]
    fn group_by_with_list_slice() {
        let prog = p("MATCH (n) RETURN n.items[..3], COUNT(*) GROUP BY n.items");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_record_literal() {
        let prog = p("MATCH (n) RETURN {x: n.x}, COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_case_simple() {
        let prog = p(
            "MATCH (n) RETURN CASE n.x WHEN 1 THEN 'one' ELSE 'other' END, COUNT(*) GROUP BY n.x",
        );
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_is_not_null() {
        let prog = p("MATCH (n) RETURN n.x IS NOT NULL, COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_cardinality() {
        let prog = p("MATCH (n) RETURN SIZE(n.items), COUNT(*) GROUP BY n.items");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_degrees() {
        let prog = p("MATCH (n) RETURN DEGREES(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_radians() {
        let prog = p("MATCH (n) RETURN RADIANS(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_or() {
        let prog = p("MATCH (n) RETURN n.a OR n.b, COUNT(*) GROUP BY n.a, n.b");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_xor() {
        let prog = p("MATCH (n) RETURN n.a XOR n.b, COUNT(*) GROUP BY n.a, n.b");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_and() {
        let prog = p("MATCH (n) RETURN n.a AND n.b, COUNT(*) GROUP BY n.a, n.b");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_compare() {
        let prog = p("MATCH (n) RETURN n.x > 0, COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_is_truth() {
        let prog = p("MATCH (n) RETURN n.active IS TRUE, COUNT(*) GROUP BY n.active");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_truncate() {
        let prog = p("MATCH (n) RETURN TRUNCATE(n.x, 2), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_round() {
        let prog = p("MATCH (n) RETURN ROUND(n.x, 2), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_atan2() {
        let prog = p("MATCH (n) RETURN ATAN2(n.x, n.y), COUNT(*) GROUP BY n.x, n.y");
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[cfg(feature = "cypher")]
    #[test]
    fn group_by_with_contains() {
        let prog = p(
            "MATCH (n) WHERE n.name CONTAINS 'A' RETURN n.name CONTAINS 'B', COUNT(*) GROUP BY n.name",
        );
        assert!(validate(&prog).is_ok(), "validate failed");
    }

    #[test]
    fn group_by_with_function_call() {
        let prog = p("MATCH (n) RETURN my_func(n.x), COUNT(*) GROUP BY n.x");
        assert!(validate(&prog).is_ok(), "validate failed");
    }
}
