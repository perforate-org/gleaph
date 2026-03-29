//! Comprehensive parser tests for the GQL parser.
//!
//! These test cases cover the full range of GQL syntax supported by the parser,
//! adapted from the old gleaph test suite and extended for GQL compliance.

use gleaph_gql::ast::{
    BindingTypeAnnotation, CompositeQueryExpr, DurationQualifier, ExprKind, GraphTypeElement,
    GraphTypeSpec, Keyword, ResultStatement, SchemaReference, SelectQuerySpecification,
    SelectSource, Statement, ValueType,
};
use gleaph_gql::parser;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql::validate::validate;

/// Parse and validate helper — panics with a useful message on failure.
fn parse_ok(input: &str) {
    let program =
        parser::parse(input).unwrap_or_else(|e| panic!("parse failed: {e}\ninput: {input}"));
    validate(&program).unwrap_or_else(|e| panic!("validate failed: {e}\ninput: {input}"));
}

fn parse_ok_syntax_only(input: &str) {
    parser::parse(input).unwrap_or_else(|e| panic!("parse failed: {e}\ninput: {input}"));
}

fn parse_validate_err(input: &str) {
    let program =
        parser::parse(input).unwrap_or_else(|e| panic!("parse failed: {e}\ninput: {input}"));
    assert!(
        validate(&program).is_err(),
        "expected validate error for: {input}"
    );
}

fn parse_program_ok(input: &str) -> gleaph_gql::ast::GqlProgram {
    parser::parse(input).unwrap_or_else(|e| panic!("parse failed: {e}\ninput: {input}"))
}

/// Expect parse to fail.
fn parse_err(input: &str) {
    assert!(
        parser::parse(input).is_err(),
        "expected parse error for: {input}"
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// Basic MATCH / RETURN
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn simple_match_return() {
    parse_ok("MATCH (n) RETURN n");
}

#[test]
fn match_with_label() {
    parse_ok("MATCH (n:User) RETURN n");
}

#[test]
fn match_with_where() {
    parse_ok("MATCH (n:User) WHERE n.age > 30 RETURN n");
}

#[test]
fn match_edge_return() {
    parse_ok("MATCH (a:User)-[e:KNOWS]->(b:User) RETURN a, b, e");
}

#[test]
fn match_edge_return_property() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b) WHERE a.name = 'Alice' RETURN b.name");
}

#[test]
fn return_star() {
    parse_ok("MATCH (n) RETURN *");
}

#[test]
fn return_with_alias() {
    parse_ok("MATCH (n) RETURN n.name AS name");
}

#[test]
#[cfg(feature = "cypher")]
fn return_no_bindings() {
    parse_ok("MATCH (n) RETURN NO BINDINGS");
}

// ════════════════════════════════════════════════════════════════════════════════
// SET / REMOVE / DELETE
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn set_property() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b) WHERE a.name = 'Alice' SET a.score = 42");
}

#[test]
fn set_property_expr() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b) SET a.score = b.score + 1");
}

#[test]
fn set_edge_property() {
    parse_ok("MATCH (a:User)-[e:KNOWS]->(b) WHERE a.name = 'X' SET e.weight = 0.5");
}

#[test]
fn set_label() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b) WHERE a.name = 'Alice' SET a:Admin");
}

#[test]
fn set_all_properties() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b) SET a = {name: 'Bob', age: 30}");
}

#[test]
fn set_all_properties_empty_map() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b) SET a = {}");
}

#[test]
fn remove_property() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b) WHERE a.name = 'Alice' REMOVE a.score");
}

#[test]
fn remove_edge_property() {
    parse_ok("MATCH (a:User)-[e:KNOWS]->(b) REMOVE e.weight");
}

#[test]
fn remove_label() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b) WHERE a.name = 'Alice' REMOVE a:Admin");
}

#[test]
fn detach_delete() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.name = 'Bob' DETACH DELETE b");
}

#[test]
fn nodetach_delete() {
    parse_ok("MATCH (a:X)-[:E]->(b) NODETACH DELETE a");
}

#[test]
fn delete_multiple_targets() {
    parse_ok("MATCH (a:X)-[:E]->(b:Y) DELETE a, b");
}

#[test]
fn delete_expression_property() {
    // GQL §13.5: deleteItem is a valueExpression, not just a variable
    parse_ok_syntax_only("MATCH (n:Foo) DELETE n.prop");
}

#[test]
fn delete_expression_complex() {
    parse_ok_syntax_only("MATCH (n)-[e]->(m) DELETE n, e, m");
}

// ════════════════════════════════════════════════════════════════════════════════
// WITH clause (cypher extension)
// ════════════════════════════════════════════════════════════════════════════════

// ════════════════════════════════════════════════════════════════════════════════
// DISTINCT / LIMIT / OFFSET
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn distinct_return() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) RETURN DISTINCT b.name");
}

#[test]
fn limit_offset() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) RETURN b.name LIMIT 10 OFFSET 5");
}

#[test]
fn skip_keyword() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) RETURN b.name SKIP 3");
}

#[test]
fn offset_keyword() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) RETURN b.name OFFSET 3");
}

// ════════════════════════════════════════════════════════════════════════════════
// UNION / EXCEPT / INTERSECT / OTHERWISE
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn union_query() {
    parse_ok(
        "MATCH (a:User)-[:KNOWS]->(b:User) RETURN a.name UNION MATCH (a:User)-[:LIKES]->(b:User) RETURN a.name",
    );
}

#[test]
fn union_all_query() {
    parse_ok(
        "MATCH (a:User)-[:KNOWS]->(b:User) RETURN a.name UNION ALL MATCH (a:User)-[:LIKES]->(b:User) RETURN a.name",
    );
}

#[test]
fn except_query() {
    parse_ok(
        "MATCH (a:User)-[:KNOWS]->(b:User) RETURN a.name EXCEPT MATCH (a:User)-[:LIKES]->(b:User) RETURN a.name",
    );
}

#[test]
fn otherwise_query() {
    parse_ok("MATCH (n:User) RETURN n.name OTHERWISE MATCH (m:User) RETURN m.name");
}

// ════════════════════════════════════════════════════════════════════════════════
// GROUP BY / HAVING / ORDER BY
// ════════════════════════════════════════════════════════════════════════════════

// GROUP BY after RETURN is no longer supported (not in GQL).
// GQL handles grouping implicitly through aggregation.

#[test]
fn order_by_asc_desc() {
    parse_ok("MATCH (n) RETURN n.name ORDER BY n.name ASC");
}

#[test]
fn order_by_nulls_last() {
    parse_ok("MATCH (n) RETURN n.score ORDER BY n.score DESC NULLS LAST");
}

#[test]
fn return_group_by_rejects_ungrouped_non_aggregate_item() {
    parse_validate_err("MATCH (n) RETURN n.name, n.age GROUP BY n.name");
}

#[test]
fn select_from_graph_match() {
    parse_ok("SELECT n.name FROM myGraph MATCH (n:Person) WHERE n.age > 21");
}

#[test]
fn select_with_group_by_and_having() {
    parse_ok("SELECT n FROM myGraph MATCH (n) GROUP BY n HAVING COUNT(*) > 1");
}

#[test]
fn select_order_by_alias() {
    parse_ok("SELECT n.name AS name FROM myGraph MATCH (n) ORDER BY name");
}

#[test]
fn select_order_by_missing_alias_rejected() {
    parse_validate_err("SELECT n.name AS name FROM myGraph MATCH (n) ORDER BY other");
}

#[test]
fn select_group_by_rejects_ungrouped_non_aggregate_item() {
    parse_validate_err("SELECT n.name, n.age FROM myGraph MATCH (n) GROUP BY n.name");
}

#[test]
fn select_group_by_rejects_ungrouped_order_by_expr() {
    parse_validate_err(
        "SELECT n.name, COUNT(*) FROM myGraph MATCH (n) GROUP BY n.name ORDER BY n.age",
    );
}

#[test]
fn select_group_by_rejects_ungrouped_having_expr() {
    parse_validate_err(
        "SELECT n.name, COUNT(*) FROM myGraph MATCH (n) GROUP BY n.name HAVING n.age > 1",
    );
}

#[test]
fn select_ast_graph_match_source() {
    let program = parse_program_ok("SELECT n FROM myGraph MATCH (n)");
    let stmt = &program
        .transaction_activity
        .as_ref()
        .unwrap()
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = stmt else {
        panic!("expected query statement");
    };
    let ResultStatement::Select(select) = query.left.result.as_ref().unwrap() else {
        panic!("expected select result");
    };
    match select.source.as_ref().unwrap() {
        SelectSource::GraphMatchList(items) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].graph.parts[0], "myGraph");
        }
        _ => panic!("expected graph match source"),
    }
    assert!(query.left.parts.is_empty());
}

#[test]
fn select_ast_graph_nested_source() {
    let program = parse_program_ok("SELECT n FROM myGraph { MATCH (n) RETURN n }");
    let stmt = &program
        .transaction_activity
        .as_ref()
        .unwrap()
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = stmt else {
        panic!("expected query statement");
    };
    let ResultStatement::Select(select) = query.left.result.as_ref().unwrap() else {
        panic!("expected select result");
    };
    match select.source.as_ref().unwrap() {
        SelectSource::QuerySpecification(SelectQuerySpecification::GraphNested {
            graph,
            query,
        }) => {
            assert_eq!(graph.parts[0], "myGraph");
            assert!(matches!(**query, CompositeQueryExpr { .. }));
        }
        _ => panic!("expected graph+nested source"),
    }
    assert!(query.left.parts.is_empty());
}

#[test]
fn select_graph_nested_with_having() {
    parse_ok("SELECT n FROM myGraph { MATCH (n) RETURN n } HAVING COUNT(*) > 1");
}

#[test]
fn select_nested_return_star_exports_bindings() {
    parse_ok("SELECT n FROM { MATCH (n) RETURN * }");
}

#[test]
fn select_graph_nested_return_star_exports_bindings() {
    parse_ok("SELECT n FROM myGraph { MATCH (n) RETURN * }");
}

#[test]
fn select_nested_union_exports_bindings() {
    parse_ok("SELECT n FROM { MATCH (n) RETURN n UNION MATCH (n) RETURN n }");
}

#[test]
fn select_ast_multi_graph_match_source() {
    let program = parse_program_ok("SELECT n FROM g1 MATCH (n), g2 MATCH (m) GROUP BY n");
    let stmt = &program
        .transaction_activity
        .as_ref()
        .unwrap()
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = stmt else {
        panic!("expected query statement");
    };
    let ResultStatement::Select(select) = query.left.result.as_ref().unwrap() else {
        panic!("expected select result");
    };
    match select.source.as_ref().unwrap() {
        SelectSource::GraphMatchList(items) => {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].graph.parts[0], "g1");
            assert_eq!(items[1].graph.parts[0], "g2");
        }
        _ => panic!("expected graph match list source"),
    }
    assert!(query.left.parts.is_empty());
}

#[test]
fn at_schema_clause_query() {
    let program = parse_program_ok("AT HOME_SCHEMA MATCH (n) RETURN n");
    let body = program
        .transaction_activity
        .as_ref()
        .unwrap()
        .body
        .as_ref()
        .unwrap();
    let Statement::Query(ref cqe) = body.first else {
        panic!("expected query")
    };
    let at_schema = cqe.left.at_schema.as_ref().expect("expected at_schema");
    assert!(matches!(at_schema, SchemaReference::Current(s) if s == "HOME_SCHEMA"));
}

#[test]
fn value_variable_definition_prefix() {
    parse_ok("VALUE x = 1 RETURN x");
}

#[test]
fn value_variable_definition_typed() {
    let program = parse_program_ok("VALUE x :: INT32 = 42 RETURN x");
    let body = program
        .transaction_activity
        .as_ref()
        .unwrap()
        .body
        .as_ref()
        .unwrap();
    let Statement::Query(ref cqe) = body.first else {
        panic!("expected query")
    };
    let binding = &cqe.left.prefix_bindings[0];
    assert!(matches!(
        binding.type_annotation,
        Some(BindingTypeAnnotation::Value(ValueType::Int32 { .. }))
    ));
}

#[test]
fn value_variable_definition_typed_keyword() {
    // TYPED keyword (equivalent to ::)
    parse_ok("VALUE x TYPED INT32 = 42 RETURN x");
}

#[test]
fn graph_variable_definition_prefix() {
    parse_ok("GRAPH g = myGraph USE g MATCH (n) RETURN n");
}

#[test]
fn graph_variable_definition_typed_any() {
    let program = parse_program_ok("GRAPH g :: ANY GRAPH = myGraph RETURN 1");
    let body = program
        .transaction_activity
        .as_ref()
        .unwrap()
        .body
        .as_ref()
        .unwrap();
    let Statement::Query(ref cqe) = body.first else {
        panic!("expected query")
    };
    let binding = &cqe.left.prefix_bindings[0];
    assert!(matches!(
        binding.type_annotation,
        Some(BindingTypeAnnotation::AnyGraph {
            not_null: false,
            ..
        })
    ));
}

#[test]
fn graph_variable_definition_typed_closed() {
    let program = parse_program_ok("GRAPH g :: GRAPH myGraphType = myGraph RETURN 1");
    let body = program
        .transaction_activity
        .as_ref()
        .unwrap()
        .body
        .as_ref()
        .unwrap();
    let Statement::Query(ref cqe) = body.first else {
        panic!("expected query")
    };
    let binding = &cqe.left.prefix_bindings[0];
    match &binding.type_annotation {
        Some(BindingTypeAnnotation::ClosedGraph {
            graph_type,
            not_null,
            ..
        }) => {
            assert_eq!(graph_type.parts[0], "myGraphType");
            assert!(!not_null);
        }
        other => panic!("expected ClosedGraph, got {other:?}"),
    }
}

#[test]
fn use_graph_rejects_value_binding_name() {
    parse_validate_err("VALUE g = 1 USE g MATCH (n) RETURN n");
}

#[test]
fn select_graph_source_rejects_value_binding_name() {
    parse_validate_err("VALUE g = 1 SELECT n FROM g MATCH (n)");
}

#[test]
fn select_graph_source_accepts_graph_binding_name() {
    parse_ok("GRAPH g = myGraph SELECT n FROM g MATCH (n)");
}

// ════════════════════════════════════════════════════════════════════════════════
// CURRENT_GRAPH / HOME_GRAPH
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn use_current_graph() {
    parse_ok("USE CURRENT_GRAPH MATCH (n) RETURN n");
}

#[test]
fn use_current_property_graph() {
    parse_ok("USE CURRENT_PROPERTY_GRAPH MATCH (n) RETURN n");
}

#[test]
fn use_home_graph() {
    parse_ok("USE HOME_GRAPH MATCH (n) RETURN n");
}

#[test]
fn use_home_property_graph() {
    parse_ok("USE HOME_PROPERTY_GRAPH MATCH (n) RETURN n");
}

#[test]
fn create_graph_any_type() {
    let program = parse_program_ok("CREATE GRAPH myGraph ANY");
    let body = program
        .transaction_activity
        .as_ref()
        .unwrap()
        .body
        .as_ref()
        .unwrap();
    let Statement::CreateGraph(ref create) = body.first else {
        panic!("expected CreateGraph")
    };
    assert!(matches!(create.graph_type, Some(GraphTypeSpec::Any { .. })));
}

#[test]
fn string_min_max_length() {
    parse_ok("CREATE GRAPH TYPE myType { (Person :Person {name STRING(5, 100)}) }");
}

#[test]
fn int_with_precision() {
    // GQL §18.9: INT(precision) is a standard GQL type
    let program = parse_program_ok("CREATE GRAPH TYPE myType { (Person :Person {age INT(10)}) }");
    let body = program
        .transaction_activity
        .as_ref()
        .unwrap()
        .body
        .as_ref()
        .unwrap();
    let Statement::CreateGraphType(ref create) = body.first else {
        panic!("expected CreateGraphType")
    };
    let GraphTypeElement::Node(ref node) = create.definition.elements[0] else {
        panic!("expected Node")
    };
    let field_ty = &node.properties[0].value_type;
    assert_eq!(
        *field_ty,
        ValueType::IntPrecision {
            keyword: Keyword::new("INT"),
            precision: 10
        }
    );
}

#[test]
fn uint_with_precision() {
    parse_ok("CREATE GRAPH TYPE myType { (Node :Node {val UINT(32)}) }");
}

#[test]
fn integer_with_precision() {
    parse_ok("CREATE GRAPH TYPE myType { (Node :Node {val INTEGER(64)}) }");
}

#[test]
fn float_with_precision_and_scale() {
    parse_ok("CREATE GRAPH TYPE myType { (Node :Node {val FLOAT(10, 2)}) }");
}

#[test]
fn signed_integer_with_precision() {
    parse_ok("CREATE GRAPH TYPE myType { (Node :Node {val SIGNED INTEGER(16)}) }");
}

#[test]
fn unsigned_integer_with_precision() {
    parse_ok("CREATE GRAPH TYPE myType { (Node :Node {val UNSIGNED INTEGER(8)}) }");
}

#[test]
fn binding_table_variable_definition_prefix() {
    parse_ok("TABLE t = myTable RETURN t");
}

#[test]
fn binding_table_nested_query_definition_prefix() {
    parse_ok("TABLE t = { MATCH (n) RETURN n } RETURN t");
}

#[test]
fn focused_nested_query_after_use_graph() {
    parse_ok_syntax_only("USE myGraph { MATCH (n) RETURN n }");
}

#[test]
fn focused_nested_data_modifying_query_after_use_graph() {
    parse_ok_syntax_only("USE myGraph { MATCH (n) SET n.flag = TRUE }");
}

#[test]
fn match_with_graph_pattern_yield() {
    parse_ok("MATCH (a)-[e]->(b) YIELD e RETURN e");
}

#[test]
fn match_yield_alias_exports_binding() {
    parse_ok("MATCH (a)-[e]->(b) YIELD e AS edge RETURN edge");
}

#[test]
fn match_yield_limits_visible_bindings() {
    parse_validate_err("MATCH (a)-[e]->(b) YIELD e RETURN a");
}

// ════════════════════════════════════════════════════════════════════════════════
// Aggregation functions
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn aggregate_count_star() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) RETURN COUNT(*)");
}

#[test]
fn aggregate_count_expr() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) RETURN COUNT(b.score)");
}

#[test]
fn aggregate_count_distinct() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) RETURN COUNT(DISTINCT b.name)");
}

#[test]
fn aggregate_sum() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) RETURN SUM(b.score)");
}

#[test]
fn aggregate_avg() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) RETURN AVG(b.score)");
}

#[test]
fn aggregate_min_max() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) RETURN MIN(b.score), MAX(b.score)");
}

#[test]
fn aggregate_collect() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) RETURN COLLECT(b.name)");
}

// ════════════════════════════════════════════════════════════════════════════════
// NULL predicates / IN
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn is_null() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.score IS NULL RETURN a.name");
}

#[test]
fn is_not_null() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.score IS NOT NULL RETURN a.name");
}

#[test]
fn null_combined_with_or() {
    parse_ok(
        "MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.score IS NULL OR b.name IS NOT NULL RETURN b.name",
    );
}

#[test]
#[cfg(feature = "sql-compat")]
fn in_list_strings() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.name IN ('Alice', 'Bob') RETURN b.name");
}

#[test]
#[cfg(feature = "sql-compat")]
fn in_list_numbers() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.score IN (1, 2, 3) RETURN b.name");
}

// ════════════════════════════════════════════════════════════════════════════════
// EXISTS subquery
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn exists_subquery() {
    parse_ok(
        "MATCH (a:User)-[:KNOWS]->(b:User) WHERE EXISTS { MATCH (b)-[:LIKES]->(c) RETURN c } RETURN a.name",
    );
}

#[test]
fn exists_pattern() {
    parse_ok("MATCH (n) WHERE EXISTS { (n)-[:KNOWS]->(m) } RETURN n");
}

// ════════════════════════════════════════════════════════════════════════════════
// CASE expressions
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn case_simple() {
    parse_ok("MATCH (a:User)-[:KNOWS]->(b) RETURN CASE a.score WHEN 1 THEN 'low' ELSE 'high' END");
}

#[test]
fn case_searched() {
    parse_ok(
        "MATCH (n) RETURN CASE WHEN n.age > 18 THEN 'adult' WHEN n.age > 12 THEN 'teen' ELSE 'child' END",
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// Path patterns and quantifiers
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn variable_length_path() {
    parse_ok("MATCH (a)-[e]->{1,3}(b) RETURN a, b");
}

#[test]
fn variable_length_path_star() {
    parse_ok("MATCH (a)-[e]->*(b) RETURN a, b");
}

#[test]
fn variable_length_path_plus() {
    parse_ok("MATCH (a)-[e]->+(b) RETURN a, b");
}

#[test]
fn variable_length_path_optional() {
    parse_ok("MATCH (a)-[e]->?(b) RETURN a, b");
}

#[test]
fn path_variable_assignment() {
    parse_ok("MATCH p = (a)-[e]->(b) RETURN p");
}

#[test]
fn subpath_fixed_quantifier() {
    parse_ok("MATCH (a)((x)-[:E]->(y)){3}(b) RETURN a, b");
}

#[test]
fn subpath_range_quantifier() {
    parse_ok("MATCH (a)((x)-[:E]->(y)){1,3}(b) RETURN a, b");
}

// ════════════════════════════════════════════════════════════════════════════════
// Search prefixes (ANY, ALL, SHORTEST)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn any_shortest() {
    parse_ok("MATCH ANY SHORTEST (a)-[e]->{1,3}(b) RETURN a, b");
}

#[test]
fn all_shortest() {
    parse_ok("MATCH ALL SHORTEST (a)-[e]->{1,3}(b) RETURN a, b");
}

#[test]
fn any_k_paths() {
    parse_ok("MATCH ANY 3 PATHS (a:User)-[:KNOWS]->(b:User) RETURN a");
}

#[test]
fn all_paths() {
    parse_ok("MATCH ALL PATHS (a)-[e]->{1,3}(b) RETURN a, b");
}

#[test]
fn trail_mode() {
    parse_ok("MATCH TRAIL (a)-[e]->{1,4}(b) RETURN a, b");
}

#[test]
fn acyclic_mode() {
    parse_ok("MATCH ACYCLIC (a)-[e]->{1,3}(b) RETURN a, b");
}

// ════════════════════════════════════════════════════════════════════════════════
// Edge directions
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn edge_left() {
    parse_ok("MATCH (a)<-[e:KNOWS]-(b) RETURN a, b");
}

#[test]
fn edge_undirected_dash() {
    parse_ok("MATCH (a)-[e:KNOWS]-(b) RETURN a, b");
}

#[test]
fn edge_undirected_tilde() {
    parse_ok("MATCH (a)~[e:KNOWS]~(b) RETURN a");
}

#[test]
fn edge_tilde_right() {
    parse_ok("MATCH (a)~[e:KNOWS]~>(b) RETURN a");
}

#[test]
fn edge_tilde_left() {
    parse_ok("MATCH (a)<~[e:KNOWS]~(b) RETURN a");
}

#[test]
fn edge_tilde_both() {
    parse_ok("MATCH (a)<~[e:KNOWS]~>(b) RETURN a");
}

#[test]
fn edge_left_right() {
    parse_ok("MATCH (a)<-[e:KNOWS]->(b) RETURN a, b");
}

// ════════════════════════════════════════════════════════════════════════════════
// Simplified path patterns
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn simplified_edge_right() {
    parse_ok("MATCH (a)-/KNOWS/->(b) RETURN a");
}

#[test]
fn simplified_edge_left() {
    parse_ok("MATCH (a)<-/KNOWS/-(b) RETURN a");
}

#[test]
fn simplified_edge_both() {
    parse_ok("MATCH (a)<-/KNOWS/->(b) RETURN a");
}

#[test]
fn simplified_edge_undirected() {
    parse_ok("MATCH (a)-/KNOWS/-(b) RETURN a");
}

#[test]
fn simplified_edge_label_or() {
    parse_ok("MATCH (a)-/KNOWS|LIKES/->(b) RETURN a");
}

#[test]
fn simplified_edge_label_not() {
    parse_ok("MATCH (a)-/!Deleted/->(b) RETURN a");
}

#[test]
fn simplified_edge_label_wildcard() {
    parse_ok("MATCH (a)-/%/->(b) RETURN a");
}

#[test]
fn simplified_edge_label_and() {
    parse_ok("MATCH (a)-/A&B/->(b) RETURN a");
}

#[test]
fn simplified_tilde_undirected() {
    parse_ok("MATCH (a)~/KNOWS/~(b) RETURN a");
}

#[test]
fn simplified_tilde_right() {
    parse_ok("MATCH (a)~/KNOWS/~>(b) RETURN a");
}

#[test]
fn simplified_tilde_left() {
    parse_ok("MATCH (a)<~/KNOWS/~(b) RETURN a");
}

// ════════════════════════════════════════════════════════════════════════════════
// Edge label expressions
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn edge_label_or() {
    parse_ok("MATCH (a)-[e:Bought|Picked]->(b) RETURN e");
}

#[test]
fn edge_label_three_way_or() {
    parse_ok("MATCH (a)-[e:Bought|Picked|Favorited]->(b) RETURN e");
}

#[test]
fn edge_label_not() {
    parse_ok("MATCH (a)-[e:!Deleted]->(b) RETURN e");
}

#[test]
fn edge_label_wildcard() {
    parse_ok("MATCH (a)-[e:%]->(b) RETURN e");
}

// ════════════════════════════════════════════════════════════════════════════════
// Graph predicates
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn is_directed() {
    parse_ok("MATCH (a)-[e:X]->(b) WHERE e IS DIRECTED RETURN e");
}

#[test]
fn is_not_directed() {
    parse_ok("MATCH (a)-[e:X]->(b) WHERE e IS NOT DIRECTED RETURN e");
}

#[test]
fn is_labeled() {
    parse_ok("MATCH (n) WHERE n IS LABELED User RETURN n");
}

#[test]
fn is_not_labeled() {
    parse_ok("MATCH (n) WHERE n IS NOT LABELED Admin RETURN n");
}

#[test]
fn is_source_of() {
    parse_ok("MATCH (a)-[e]->(b) WHERE a IS SOURCE OF e RETURN a");
}

#[test]
fn is_destination_of() {
    parse_ok("MATCH (a)-[e]->(b) WHERE b IS DESTINATION OF e RETURN b");
}

#[test]
fn all_different() {
    parse_ok("MATCH (a)-[e1]->(b)-[e2]->(c) RETURN ALL_DIFFERENT(a, b, c)");
}

#[test]
fn same() {
    parse_ok("MATCH (a)-[e]->(b) RETURN SAME(a.name, b.name)");
}

#[test]
fn property_exists() {
    parse_ok("MATCH (n) WHERE PROPERTY_EXISTS(n, name) RETURN n");
}

// ════════════════════════════════════════════════════════════════════════════════
// Truth value tests
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn is_true() {
    parse_ok("MATCH (n) WHERE (n.active = TRUE) IS TRUE RETURN n");
}

#[test]
fn is_not_false() {
    parse_ok("MATCH (n) WHERE (n.x > 0) IS NOT FALSE RETURN n");
}

#[test]
fn is_unknown() {
    parse_ok("MATCH (n) WHERE n.age IS UNKNOWN RETURN n");
}

// ════════════════════════════════════════════════════════════════════════════════
// VALUE subquery / LET IN
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn value_subquery() {
    parse_ok(
        "MATCH (a:User) RETURN a.name, VALUE { MATCH (a)-[:KNOWS]->(b) RETURN COUNT(*) } AS cnt",
    );
}

#[test]
fn let_in_expr() {
    // Test expression-level LET..IN directly via parse_expr
    let e = parser::parse_expr("LET x = 42 IN x + 1 END");
    assert!(e.is_ok(), "LET..IN expr failed: {}", e.unwrap_err());
}

#[test]
fn let_in_multiple_bindings() {
    let e = parser::parse_expr("LET x = 1, y = 2 IN x + y END");
    assert!(e.is_ok(), "LET..IN multi failed: {}", e.unwrap_err());
}

// ════════════════════════════════════════════════════════════════════════════════
// FILTER / LET / FOR / FINISH
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn filter_statement() {
    parse_ok("MATCH (n:User) FILTER n.name = 'Alice' RETURN n");
}

#[test]
fn let_statement() {
    parse_ok("MATCH (n:User) LET x = n.score * 2 RETURN n.name, x");
}

#[test]
fn for_statement() {
    parse_ok("MATCH (n) FOR x IN [1, 2, 3] RETURN n, x");
}

#[test]
fn finish_statement() {
    // FINISH is like RETURN NO BINDINGS
    parse_ok("MATCH (n:User) FINISH");
}

// ════════════════════════════════════════════════════════════════════════════════
// INSERT
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn insert_multiple_patterns() {
    parse_ok("INSERT (:A {x: 1}), (:B {y: 2}), (:C)");
}

#[test]
fn insert_node_and_edge() {
    parse_ok("INSERT (:Tag), (:A)-[:X]->(:B)");
}

// ════════════════════════════════════════════════════════════════════════════════
// DDL: Schema / Graph / Graph Type
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn create_schema() {
    parse_ok("CREATE SCHEMA /mySchema");
}

#[test]
fn drop_schema() {
    parse_ok("DROP SCHEMA /mySchema");
}

#[test]
fn create_schema_if_not_exists() {
    parse_ok("CREATE SCHEMA IF NOT EXISTS /mySchema");
}

#[test]
fn drop_schema_if_exists() {
    parse_ok("DROP SCHEMA IF EXISTS /mySchema");
}

#[test]
fn create_graph() {
    parse_ok("CREATE GRAPH myGraph");
}

#[test]
fn create_graph_if_not_exists() {
    parse_ok("CREATE GRAPH IF NOT EXISTS myGraph");
}

#[test]
fn drop_graph() {
    parse_ok("DROP GRAPH myGraph");
}

#[test]
fn drop_graph_if_exists() {
    parse_ok("DROP GRAPH IF EXISTS myGraph");
}

#[test]
fn create_property_graph() {
    parse_ok("CREATE PROPERTY GRAPH myGraph");
}

#[test]
fn drop_property_graph() {
    parse_ok("DROP PROPERTY GRAPH myGraph");
}

#[test]
fn create_graph_type_pattern_body_ast() {
    let program =
        parse_program_ok("CREATE GRAPH TYPE myType {(A :A {x INT32}), (B :B), (A)-[R :R]->(B)}");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::CreateGraphType(stmt) = statement else {
        panic!("expected create graph type statement, got {statement:?}");
    };
    assert_eq!(stmt.definition.elements.len(), 3);

    let GraphTypeElement::Node(node) = &stmt.definition.elements[0] else {
        panic!("expected first graph type element to be a node");
    };
    assert_eq!(node.name.as_deref(), Some("A"));
    assert_eq!(node.label_set.as_ref().unwrap().labels, vec!["A"]);
    assert_eq!(node.properties.len(), 1);
    assert_eq!(node.properties[0].name, "x");
    assert_eq!(
        node.properties[0].value_type,
        ValueType::Int32 {
            keyword: Keyword::new("INT32")
        }
    );

    let GraphTypeElement::Edge(edge) = &stmt.definition.elements[2] else {
        panic!("expected third graph type element to be an edge");
    };
    assert_eq!(edge.name.as_deref(), Some("R"));
    assert_eq!(edge.direction, EdgeDirection::PointingRight);
    assert_eq!(edge.source.type_name.as_deref(), Some("A"));
    assert_eq!(edge.destination.type_name.as_deref(), Some("B"));
    assert_eq!(edge.label_set.as_ref().unwrap().labels, vec!["R"]);
}

#[test]
fn create_graph_type_phrase_node_ast() {
    let program = parse_program_ok(
        "CREATE GRAPH TYPE myType { NODE Person LABELS Person&Employee {name STRING(1, 100), code CHAR(8)} AS p }",
    );
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::CreateGraphType(stmt) = statement else {
        panic!("expected create graph type statement, got {statement:?}");
    };

    let [GraphTypeElement::Node(node)] = &stmt.definition.elements[..] else {
        panic!("expected one phrase node element");
    };
    assert_eq!(node.name.as_deref(), Some("Person"));
    assert_eq!(node.alias.as_deref(), Some("p"));
    assert_eq!(
        node.label_set.as_ref().unwrap().labels,
        vec!["Person", "Employee"]
    );
    assert_eq!(node.properties.len(), 2);
    assert_eq!(node.properties[0].name, "name");
    assert_eq!(
        node.properties[0].value_type,
        ValueType::String {
            min_length: Some(1),
            max_length: Some(100),
        }
    );
    assert_eq!(node.properties[1].name, "code");
    assert_eq!(
        node.properties[1].value_type,
        ValueType::Char {
            keyword: Keyword::new("CHAR"),
            length: Some(8)
        }
    );
}

#[test]
fn create_graph_type_phrase_edge_ast() {
    let program = parse_program_ok(
        "CREATE GRAPH TYPE myType { DIRECTED EDGE WorksFor LABEL ReportsTo {since INT32, ratio DECIMAL(5, 2)} CONNECTING (employee -> manager) }",
    );
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::CreateGraphType(stmt) = statement else {
        panic!("expected create graph type statement, got {statement:?}");
    };

    let [GraphTypeElement::Edge(edge)] = &stmt.definition.elements[..] else {
        panic!("expected one phrase edge element");
    };
    assert_eq!(edge.name.as_deref(), Some("WorksFor"));
    assert_eq!(edge.direction, EdgeDirection::PointingRight);
    assert_eq!(edge.label_set.as_ref().unwrap().labels, vec!["ReportsTo"]);
    assert_eq!(edge.source.type_name.as_deref(), Some("employee"));
    assert_eq!(edge.destination.type_name.as_deref(), Some("manager"));
    assert_eq!(edge.properties.len(), 2);
    assert_eq!(edge.properties[0].name, "since");
    assert_eq!(
        edge.properties[0].value_type,
        ValueType::Int32 {
            keyword: Keyword::new("INT32")
        }
    );
    assert_eq!(edge.properties[1].name, "ratio");
    assert_eq!(
        edge.properties[1].value_type,
        ValueType::Decimal {
            keyword: Keyword::new("DECIMAL"),
            precision: Some(5),
            scale: Some(2),
        }
    );
}

#[test]
fn create_graph_type_phrase_left_edge_ast() {
    let program = parse_program_ok(
        "CREATE GRAPH TYPE myType { DIRECTED EDGE ManagedBy LABEL ManagedBy CONNECTING (manager <- employee) }",
    );
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::CreateGraphType(stmt) = statement else {
        panic!("expected create graph type statement, got {statement:?}");
    };

    let [GraphTypeElement::Edge(edge)] = &stmt.definition.elements[..] else {
        panic!("expected one phrase edge element");
    };
    assert_eq!(edge.direction, EdgeDirection::PointingRight);
    assert_eq!(edge.source.type_name.as_deref(), Some("employee"));
    assert_eq!(edge.destination.type_name.as_deref(), Some("manager"));
}

#[test]
fn create_graph_type_duplicate_property_rejected() {
    parse_validate_err(
        "CREATE GRAPH TYPE myType { NODE Person LABELS Person {name STRING, name INT32} }",
    );
}

#[test]
fn create_graph_type_duplicate_alias_rejected() {
    parse_validate_err(
        "CREATE GRAPH TYPE myType { NODE Person LABELS Person AS p, NODE Company LABELS Company AS p }",
    );
}

#[test]
fn create_graph_with_inline_graph_type_validates() {
    parse_ok(
        "CREATE GRAPH myGraph { NODE Person LABELS Person {name STRING} AS employee, NODE Manager LABELS Manager AS manager, DIRECTED EDGE WorksFor LABEL WorksFor CONNECTING (employee -> manager) }",
    );
}

#[test]
fn create_graph_with_inline_graph_type_duplicate_property_rejected() {
    parse_validate_err(
        "CREATE GRAPH myGraph { NODE Person LABELS Person {name STRING, name INT32} }",
    );
}

#[test]
fn create_graph_type_edge_endpoint_matches_declared_node_refs() {
    parse_ok(
        "CREATE GRAPH TYPE myType { NODE Person LABELS Person AS employee, NODE Manager LABELS Manager AS manager, DIRECTED EDGE WorksFor LABEL WorksFor CONNECTING (employee -> manager) }",
    );
}

#[test]
fn create_graph_type_edge_endpoint_unknown_node_ref_rejected() {
    parse_validate_err(
        "CREATE GRAPH TYPE myType { NODE Person LABELS Person AS employee, DIRECTED EDGE WorksFor LABEL WorksFor CONNECTING (employee -> boss) }",
    );
}

#[test]
fn create_graph_type_edge_endpoint_ambiguous_node_ref_rejected() {
    parse_validate_err(
        "CREATE GRAPH TYPE myType { NODE Person LABELS Person, NODE Employee LABELS Person, DIRECTED EDGE WorksFor LABEL WorksFor CONNECTING (Person -> Person) }",
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// DDL semantic validation
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn create_graph_if_not_exists_or_replace_rejected() {
    // IF NOT EXISTS and OR REPLACE are mutually exclusive.
    parse_validate_err("CREATE OR REPLACE GRAPH IF NOT EXISTS myGraph {}");
}

#[test]
fn create_graph_type_if_not_exists_or_replace_rejected() {
    parse_validate_err("CREATE OR REPLACE GRAPH TYPE IF NOT EXISTS myType {}");
}

// ════════════════════════════════════════════════════════════════════════════════
// Transaction semantic validation
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn start_transaction_contradictory_modes_rejected() {
    parse_validate_err("START TRANSACTION READ ONLY, READ WRITE RETURN 1");
}

#[test]
fn start_transaction_single_mode_ok() {
    parse_ok("START TRANSACTION READ ONLY RETURN 1");
    parse_ok("START TRANSACTION READ WRITE RETURN 1");
}

// ════════════════════════════════════════════════════════════════════════════════
// CALL semantic validation
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn call_yield_duplicate_alias_rejected() {
    parse_validate_err("CALL myProc() YIELD x AS a, y AS a RETURN a");
}

#[test]
fn call_yield_unique_aliases_ok() {
    parse_ok("CALL myProc() YIELD x AS a, y AS b RETURN a, b");
}

#[test]
fn inline_call_duplicate_scope_vars_rejected() {
    parse_validate_err("MATCH (n) CALL (n, n) { RETURN n } RETURN n");
}

#[test]
fn match_yield_duplicate_alias_rejected() {
    parse_validate_err("MATCH (a)-[e]->(b) YIELD a AS x, b AS x RETURN x");
}

// ════════════════════════════════════════════════════════════════════════════════
// CAST
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn cast_int_to_float() {
    parse_ok("RETURN CAST(42 AS FLOAT)");
}

#[test]
fn cast_to_string_with_length() {
    parse_ok("RETURN CAST('hello' AS STRING(10))");
}

#[test]
fn cast_to_varchar() {
    parse_ok("RETURN CAST('hello' AS VARCHAR(10))");
}

#[test]
fn cast_to_char() {
    parse_ok("RETURN CAST('hello' AS CHAR(5))");
}

#[test]
fn cast_to_bytes() {
    parse_ok("RETURN CAST('4142' AS BYTES(10))");
}

#[test]
fn cast_to_binary() {
    parse_ok("RETURN CAST('4142' AS BINARY(2))");
}

// ════════════════════════════════════════════════════════════════════════════════
// Parameters
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn param_in_where() {
    parse_ok("MATCH (n) WHERE n.age > $minAge RETURN n");
}

#[test]
fn param_in_limit() {
    parse_ok("MATCH (n) RETURN n LIMIT $limit");
}

// ════════════════════════════════════════════════════════════════════════════════
// CALL procedure
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn call_procedure_with_yield() {
    parse_ok("CALL bfs(42, 5) YIELD vertex_id, distance");
}

#[test]
fn call_procedure_yield_alias_exports_binding() {
    parse_ok("MATCH (n) CALL myproc() YIELD x AS result RETURN n, result");
}

#[test]
fn call_procedure_without_yield_does_not_export_binding() {
    parse_validate_err("MATCH (n) CALL myproc() RETURN x");
}

#[test]
fn call_procedure_without_yield() {
    parse_ok("CALL bfs(0)");
}

#[test]
fn optional_call() {
    parse_ok("OPTIONAL CALL myproc() YIELD x");
}

#[test]
fn inline_call_with_scope_clause() {
    parse_ok("MATCH (n)-[:KNOWS]->(m) CALL (n, m) { RETURN n, m } RETURN n, m");
}

#[test]
fn inline_call_scope_clause_rejects_missing_outer_binding() {
    parse_validate_err("MATCH (n) CALL (m) { RETURN m } RETURN n");
}

#[test]
fn inline_call_scope_clause_limits_body_visibility() {
    parse_validate_err("MATCH (n)-[:KNOWS]->(m) CALL (n) { RETURN m } RETURN n");
}

#[test]
fn inline_call_exports_result_bindings() {
    parse_ok("MATCH (n) CALL { RETURN 1 AS x } RETURN x");
}

#[test]
fn inline_call_scope_clause_exports_result_bindings() {
    parse_ok("MATCH (n) CALL (n) { RETURN n AS x } RETURN x");
}

#[test]
fn inline_call_exports_star_bindings() {
    parse_ok("MATCH (n) CALL { RETURN * } RETURN n");
}

#[test]
fn inline_call_scope_clause_exports_star_bindings() {
    parse_ok("MATCH (n)-[:KNOWS]->(m) CALL (n) { RETURN * } RETURN n");
}

#[test]
fn inline_call_exports_union_result_bindings() {
    parse_ok("MATCH (n) CALL { RETURN n AS x UNION RETURN n AS x } RETURN x");
}

#[test]
fn inline_call_rejects_union_binding_mismatch() {
    parse_validate_err("MATCH (n) CALL { RETURN n AS x UNION RETURN n AS y } RETURN x");
}

// ════════════════════════════════════════════════════════════════════════════════
// Session / Transaction
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn session_set_graph() {
    parse_ok("SESSION SET GRAPH myGraph MATCH (n) RETURN n");
}

#[test]
fn session_set_schema() {
    parse_ok("SESSION SET SCHEMA /mySchema MATCH (n) RETURN n");
}

#[test]
fn start_transaction() {
    parse_ok("START TRANSACTION MATCH (n) RETURN n COMMIT");
}

#[test]
fn start_transaction_read_only() {
    parse_ok("START TRANSACTION READ ONLY MATCH (n) RETURN n COMMIT");
}

#[test]
fn rollback() {
    parse_ok("START TRANSACTION MATCH (n) RETURN n ROLLBACK");
}

// ════════════════════════════════════════════════════════════════════════════════
// NEXT pipeline
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn next_pipeline() {
    // NEXT starts a new statement; variables from first RETURN don't carry into second.
    let program = parse_program_ok(
        "MATCH (a:User)-[:KNOWS]->(b) RETURN a.name AS name, b NEXT MATCH (m)-[:LIKES]->(c) RETURN m, c",
    );
    let body = program
        .transaction_activity
        .as_ref()
        .unwrap()
        .body
        .as_ref()
        .unwrap();
    // First statement exists.
    assert!(matches!(body.first, Statement::Query(_)));
    // One NEXT-chained statement.
    assert_eq!(body.next.len(), 1);
    assert!(body.next[0].yield_items.is_none());
    assert!(matches!(body.next[0].statement, Statement::Query(_)));
}

#[test]
fn next_pipeline_with_yield() {
    let program =
        parse_program_ok("MATCH (n) RETURN n NEXT YIELD n AS m MATCH (m)-[:KNOWS]->(o) RETURN o");
    let body = program
        .transaction_activity
        .as_ref()
        .unwrap()
        .body
        .as_ref()
        .unwrap();
    assert_eq!(body.next.len(), 1);
    let yield_items = body.next[0]
        .yield_items
        .as_ref()
        .expect("expected YIELD clause");
    assert_eq!(yield_items.len(), 1);
    assert_eq!(yield_items[0].alias.as_deref(), Some("m"));
}

// ════════════════════════════════════════════════════════════════════════════════
// String functions
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn function_upper_lower() {
    parse_ok("RETURN UPPER('hello'), LOWER('WORLD')");
}

#[test]
fn function_trim() {
    parse_ok("RETURN TRIM(BOTH ' ' FROM '  hello  ')");
}

#[test]
fn function_char_length() {
    parse_ok("RETURN CHAR_LENGTH('hello')");
}

// ════════════════════════════════════════════════════════════════════════════════
// Numeric functions
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn function_abs() {
    parse_ok("RETURN ABS(-5)");
}

#[test]
fn function_floor_ceil() {
    parse_ok("RETURN FLOOR(3.7), CEIL(3.2)");
}

#[test]
fn function_sqrt() {
    parse_ok("RETURN SQRT(16)");
}

#[test]
fn function_power() {
    parse_ok("RETURN POWER(2, 10)");
}

#[test]
fn function_log() {
    parse_ok("RETURN LOG(10, 100)");
}

#[test]
#[cfg(feature = "sql-compat")]
fn function_round() {
    parse_ok("RETURN ROUND(3.14159, 2)");
}

// ════════════════════════════════════════════════════════════════════════════════
// Datetime functions
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn current_date_time() {
    parse_ok("RETURN CURRENT_DATE, CURRENT_TIME, CURRENT_TIMESTAMP");
}

#[test]
fn session_user() {
    parse_ok("RETURN SESSION_USER");
}

// ════════════════════════════════════════════════════════════════════════════════
// Path/graph element functions
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn function_elements() {
    parse_ok("MATCH p = (a)-[e]->(b) RETURN ELEMENTS(p)");
}

#[test]
#[cfg(feature = "cypher")]
fn function_nodes_edges() {
    parse_ok("MATCH p = (a)-[e]->(b) RETURN NODES(p), EDGES(p)");
}

#[test]
#[cfg(feature = "cypher")]
fn function_labels() {
    parse_ok("MATCH (n) RETURN LABELS(n)");
}

#[test]
#[cfg(feature = "cypher")]
fn function_source_destination() {
    parse_ok("MATCH ()-[e]->() RETURN SOURCE(e), DESTINATION(e)");
}

#[test]
fn function_element_id() {
    parse_ok("MATCH (n) RETURN ELEMENT_ID(n)");
}

// ════════════════════════════════════════════════════════════════════════════════
// COALESCE / NULLIF
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn coalesce() {
    parse_ok("MATCH (n) RETURN COALESCE(n.name, 'unknown')");
}

#[test]
fn nullif() {
    parse_ok("MATCH (n) RETURN NULLIF(n.score, 0)");
}

// ════════════════════════════════════════════════════════════════════════════════
// List operations
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn list_literal() {
    parse_ok("RETURN [1, 2, 3]");
}

#[test]
fn list_constructor_ast() {
    let program = parse_program_ok("RETURN LIST[1, 2, 3]");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(
        items[0].expr.kind,
        ExprKind::ListConstructor { .. }
    ));
}

#[test]
#[cfg(feature = "cypher")]
fn list_index() {
    parse_ok("MATCH (n) RETURN n.scores[0]");
}

// ════════════════════════════════════════════════════════════════════════════════
// Record literal
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn record_literal() {
    parse_ok("RETURN {name: 'Alice', age: 30}");
}

#[test]
fn record_constructor_ast() {
    let program = parse_program_ok("RETURN RECORD {name: 'Alice', age: 30}");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::RecordConstructor(_)));
}

#[test]
fn value_subquery_ast() {
    let program = parse_program_ok("RETURN VALUE { MATCH (n) RETURN n }");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::ValueSubquery(_)));
}

#[test]
fn duration_between_ast() {
    let program = parse_program_ok("RETURN DURATION_BETWEEN(DATE '2023-01-01', DATE '2024-01-01')");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(
        items[0].expr.kind,
        ExprKind::DurationBetween {
            qualifier: None,
            ..
        }
    ));
}

#[test]
fn duration_between_year_to_month_ast() {
    let program = parse_program_ok(
        "RETURN DURATION_BETWEEN(DATE '2023-01-01', DATE '2024-01-01') YEAR TO MONTH",
    );
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(
        items[0].expr.kind,
        ExprKind::DurationBetween {
            qualifier: Some(DurationQualifier::YearToMonth),
            ..
        }
    ));
}

#[test]
fn duration_between_day_to_second_ast() {
    let program = parse_program_ok(
        "RETURN DURATION_BETWEEN(DATE '2023-01-01', DATE '2024-01-01') DAY TO SECOND",
    );
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(
        items[0].expr.kind,
        ExprKind::DurationBetween {
            qualifier: Some(DurationQualifier::DayToSecond),
            ..
        }
    ));
}

#[test]
fn duration_literal_ast() {
    let program = parse_program_ok("RETURN DURATION 'P1Y2M'");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::DurationLiteral(_)));
}

#[test]
fn duration_function_ast() {
    let program = parse_program_ok("RETURN DURATION({years: 1, months: 2})");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::DurationFunction(_)));
}

#[test]
fn date_literal_ast() {
    let program = parse_program_ok("RETURN DATE '2023-01-01'");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::DateLiteral(_)));
}

#[test]
fn date_function_ast() {
    let program = parse_program_ok("RETURN DATE('2023-01-01')");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::DateFunction(_)));
}

#[test]
fn datetime_literal_ast() {
    let program = parse_program_ok("RETURN DATETIME '2023-01-01T12:30:00'");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::DatetimeLiteral(_)));
}

#[test]
fn timestamp_literal_ast() {
    let program = parse_program_ok("RETURN TIMESTAMP '2023-01-01T12:30:00'");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::TimestampLiteral(_)));
}

#[test]
fn time_literal_ast() {
    let program = parse_program_ok("RETURN TIME '12:30:00'");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::TimeLiteral(_)));
}

#[test]
fn current_date_ast() {
    let program = parse_program_ok("RETURN CURRENT_DATE");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::CurrentDate));
}

#[test]
fn current_time_ast() {
    let program = parse_program_ok("RETURN CURRENT_TIME");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::CurrentTime));
}

#[test]
fn current_timestamp_ast() {
    let program = parse_program_ok("RETURN CURRENT_TIMESTAMP");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::CurrentTimestamp));
}

#[test]
fn local_time_bare_ast() {
    let program = parse_program_ok("RETURN LOCAL_TIME");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::CurrentLocalTime));
}

#[test]
fn local_time_empty_function_ast() {
    let program = parse_program_ok("RETURN LOCAL_TIME()");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    match &items[0].expr.kind {
        ExprKind::LocalTimeFunction(args) => assert!(args.is_empty()),
        other => panic!("expected LocalTimeFunction, got {other:?}"),
    }
}

#[test]
fn local_time_function_ast() {
    let program = parse_program_ok("RETURN LOCAL_TIME('12:30:00')");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::LocalTimeFunction(_)));
}

#[test]
fn local_timestamp_bare_ast() {
    let program = parse_program_ok("RETURN LOCAL_TIMESTAMP");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(
        items[0].expr.kind,
        ExprKind::CurrentLocalTimestamp
    ));
}

#[test]
fn zoned_time_function_ast() {
    let program = parse_program_ok("RETURN ZONED_TIME('12:30:00Z')");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(items[0].expr.kind, ExprKind::ZonedTimeFunction(_)));
}

#[test]
fn zoned_datetime_function_ast() {
    let program = parse_program_ok("RETURN ZONED_DATETIME('2023-01-01T12:30:00Z')");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(
        items[0].expr.kind,
        ExprKind::ZonedDatetimeFunction(_)
    ));
}

#[test]
fn zoned_time_requires_parentheses() {
    parse_err("RETURN ZONED_TIME");
}

#[test]
fn zoned_datetime_requires_parentheses() {
    parse_err("RETURN ZONED_DATETIME");
}

#[test]
fn local_datetime_requires_parentheses() {
    parse_err("RETURN LOCAL_DATETIME");
}

#[test]
fn local_datetime_function_ast() {
    let program = parse_program_ok("RETURN LOCAL_DATETIME('2023-01-01T12:30:00')");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    assert!(matches!(
        items[0].expr.kind,
        ExprKind::LocalDatetimeFunction(_)
    ));
}

#[test]
fn local_time_record_arg_ast() {
    let program = parse_program_ok("RETURN LOCAL_TIME({hour: 12, minute: 30})");
    let statement = &program
        .transaction_activity
        .as_ref()
        .expect("expected transaction activity")
        .body
        .as_ref()
        .unwrap()
        .first;
    let Statement::Query(query) = statement else {
        panic!("expected query statement, got {statement:?}");
    };
    let Some(ResultStatement::Return(ret)) = &query.left.result else {
        panic!("expected RETURN result");
    };
    let gleaph_gql::ast::ReturnBody::Items { items, .. } = &ret.body else {
        panic!("expected RETURN items");
    };
    match &items[0].expr.kind {
        ExprKind::LocalTimeFunction(args) => {
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::RecordLiteral(_)));
        }
        other => panic!("expected LocalTimeFunction, got {other:?}"),
    }
}

#[test]
fn current_date_rejects_parentheses() {
    parse_err("RETURN CURRENT_DATE()");
}

#[test]
fn current_time_rejects_parentheses() {
    parse_err("RETURN CURRENT_TIME()");
}

#[test]
fn current_timestamp_rejects_parentheses() {
    parse_err("RETURN CURRENT_TIMESTAMP()");
}

// ════════════════════════════════════════════════════════════════════════════════
// String predicates
// ════════════════════════════════════════════════════════════════════════════════

#[test]
#[cfg(feature = "cypher")]
fn contains() {
    parse_ok("MATCH (n) WHERE n.name CONTAINS 'test' RETURN n");
}

// LIKE is no longer supported (not in GQL).

// ════════════════════════════════════════════════════════════════════════════════
// Validation errors
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn unbound_var_in_return_rejected() {
    let program = parser::parse("MATCH (n) RETURN m").unwrap();
    assert!(validate(&program).is_err());
}

#[test]
fn unbound_var_in_where_rejected() {
    let program = parser::parse("MATCH (n) WHERE x > 1 RETURN n").unwrap();
    assert!(validate(&program).is_err());
}

#[test]
fn path_quantifier_invalid_range() {
    let program = parser::parse("MATCH (a)-[e]->{5,2}(b) RETURN a").unwrap();
    assert!(validate(&program).is_err());
}

// ════════════════════════════════════════════════════════════════════════════════
// Parse errors (syntax)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn unterminated_string() {
    parse_err("MATCH (n) WHERE n.name = 'unterminated RETURN n");
}

#[test]
fn missing_return_expr() {
    // Empty RETURN with no * or NO BINDINGS should fail
    parse_err("MATCH (n) RETURN");
}
