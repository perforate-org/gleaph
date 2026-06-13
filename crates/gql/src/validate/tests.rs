use super::ddl::{
    validate_create_graph, validate_create_graph_type, validate_create_schema, validate_drop_name,
};
use super::dml::{validate_delete, validate_remove_items, validate_set_items};
use super::expr::validate_let;
use super::graph_type::validate_graph_type_definition;
use super::procedure::validate_call_procedure;
use super::query_validation::{validate_graph_reference, validate_return, validate_select};
use super::session::validate_session_set;
use super::*;
use crate::parser;
use crate::token::Span;

/// Parse then validate, returning the validation result.
fn parse_and_validate(input: &str) -> VResult {
    let program = parser::parse(input).expect("parse should succeed");
    validate(&program)
}

#[test]
fn valid_simple_query() {
    assert!(parse_and_validate("MATCH (n) RETURN n").is_ok());
}

#[test]
fn valid_match_where_return() {
    assert!(parse_and_validate("MATCH (n) WHERE n.age > 30 RETURN n").is_ok());
}

#[test]
fn valid_match_edge_return() {
    assert!(parse_and_validate("MATCH (a)-[e]->(b) RETURN a, b, e").is_ok());
}

#[test]
fn valid_return_star() {
    assert!(parse_and_validate("MATCH (n) RETURN *").is_ok());
}

#[test]
fn valid_match_with_alias() {
    assert!(parse_and_validate("MATCH (n) RETURN n.name AS name").is_ok());
}

#[test]
fn unbound_variable_in_return() {
    let result = parse_and_validate("MATCH (n) RETURN m");
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("'m'"), "expected mention of 'm': {msg}");
}

#[test]
fn unbound_variable_in_where() {
    let result = parse_and_validate("MATCH (n) WHERE x > 1 RETURN n");
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("'x'"), "expected mention of 'x': {msg}");
}

#[test]
fn valid_order_by_alias() {
    assert!(parse_and_validate("MATCH (n) RETURN n.age AS age ORDER BY age").is_ok());
}

#[test]
fn invalid_return_group_by_ungrouped_non_aggregate_item() {
    let err = parse_and_validate("MATCH (n) RETURN n.name, n.age GROUP BY n.name")
        .expect_err("expected ungrouped non-aggregate RETURN item to fail");
    assert!(
        err.to_string()
            .contains("RETURN item must be grouped or aggregated when GROUP BY is present"),
        "unexpected error: {err}"
    );
}

#[test]
fn valid_named_call_yield_alias_exports_binding() {
    assert!(
        parse_and_validate("MATCH (n) CALL myproc() YIELD x AS result RETURN n, result").is_ok()
    );
}

#[test]
fn invalid_named_call_without_yield_binding_not_in_scope() {
    let err = parse_and_validate("MATCH (n) CALL myproc() RETURN x")
        .expect_err("expected non-yielded procedure binding to be unavailable");
    assert!(
        err.to_string().contains("variable 'x' is not in scope"),
        "unexpected error: {err}"
    );
}

#[test]
fn valid_let_binding() {
    assert!(parse_and_validate("MATCH (n) LET x = n.age RETURN x").is_ok());
}

#[test]
fn parameter_always_in_scope() {
    assert!(parse_and_validate("MATCH (n) WHERE n.age > $minAge RETURN n").is_ok());
}

#[test]
fn valid_exists_subquery() {
    assert!(
        parse_and_validate("MATCH (n) WHERE EXISTS { MATCH (n)-[]->(m) RETURN m } RETURN n")
            .is_ok()
    );
}

#[test]
fn path_quantifier_lower_exceeds_upper() {
    let result = parse_and_validate("MATCH (a)-[e]->{5,2}(b) RETURN a");
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("lower bound"),
        "expected quantifier error: {msg}"
    );
}

#[test]
fn valid_path_quantifier() {
    assert!(parse_and_validate("MATCH (a)-[e]->{1,5}(b) RETURN a, b").is_ok());
}

#[test]
fn delete_empty_target_caught_at_parse() {
    // DELETE with no variables should fail at parse time, but if constructed
    // directly it should fail validation.
    let del = DeleteStatement {
        span: Span::DUMMY,
        detach: DeleteDetach::Unspecified,
        items: vec![],
    };
    assert!(validate_delete(&del).is_err());
}

#[test]
fn set_empty_items_caught() {
    let set = SetStatement {
        span: Span::DUMMY,
        items: vec![],
    };
    assert!(validate_set_items(&set.items).is_err());
}

#[test]
fn remove_empty_items_caught() {
    let rem = RemoveStatement {
        span: Span::DUMMY,
        items: vec![],
    };
    assert!(validate_remove_items(&rem.items).is_err());
}

#[test]
fn valid_multiple_matches() {
    assert!(parse_and_validate("MATCH (a) MATCH (a)-[e]->(b) RETURN a, b, e").is_ok());
}

#[test]
fn valid_value_prefix_binding_in_scope() {
    assert!(parse_and_validate("VALUE x = 1 RETURN x").is_ok());
}

#[test]
fn valid_table_prefix_binding_in_scope() {
    assert!(parse_and_validate("TABLE t = myTable RETURN t").is_ok());
}

#[test]
fn invalid_use_graph_with_value_binding_name() {
    let err = parse_and_validate("VALUE g = 1 USE g MATCH (n) RETURN n")
        .expect_err("expected non-graph binding used as graph reference to fail");
    assert!(
        err.to_string()
            .contains("cannot be used as a graph reference"),
        "unexpected error: {err}"
    );
}

#[test]
fn valid_use_graph_with_graph_binding_name() {
    assert!(parse_and_validate("GRAPH g = myGraph USE g MATCH (n) RETURN n").is_ok());
}

#[test]
fn valid_inline_procedure_scope_clause() {
    assert!(
        parse_and_validate("MATCH (n)-[:KNOWS]->(m) CALL (n, m) { RETURN n, m } RETURN n, m")
            .is_ok()
    );
}

#[test]
fn invalid_inline_procedure_scope_var_missing() {
    let err = parse_and_validate("MATCH (n) CALL (m) { RETURN m } RETURN n")
        .expect_err("expected missing inline scope var to fail");
    assert!(
        err.to_string()
            .contains("inline procedure scope variable 'm' is not in scope"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_inline_procedure_scope_var_not_visible_in_body() {
    let err = parse_and_validate("MATCH (n)-[:KNOWS]->(m) CALL (n) { RETURN m } RETURN n")
        .expect_err("expected body visibility to be limited by scope clause");
    assert!(
        err.to_string().contains("variable 'm' is not in scope"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_inline_procedure_empty_scope_hides_outer_vars() {
    let err = parse_and_validate("MATCH (n) CALL () { RETURN n } RETURN n")
        .expect_err("expected explicit empty inline scope to hide outer vars");
    assert!(
        err.to_string().contains("variable 'n' is not in scope"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_inline_procedure_scope_var_hides_graph_binding_in_body() {
    let err = parse_and_validate(
        "GRAPH g = myGraph VALUE x = 1 CALL (x) { VALUE g = 1 USE g MATCH (n) RETURN n } RETURN x",
    )
    .expect_err("expected graph binding to be hidden by inline CALL scope clause");
    assert!(
        err.to_string()
            .contains("cannot be used as a graph reference"),
        "unexpected error: {err}"
    );
}

#[test]
fn valid_inline_procedure_exports_result_bindings() {
    assert!(parse_and_validate("MATCH (n) CALL { RETURN 1 AS x } RETURN x").is_ok());
}

#[test]
fn valid_inline_procedure_scope_clause_exports_result_bindings() {
    assert!(parse_and_validate("MATCH (n) CALL (n) { RETURN n AS x } RETURN x").is_ok());
}

#[test]
fn valid_inline_procedure_exports_star_bindings() {
    assert!(parse_and_validate("MATCH (n) CALL { RETURN * } RETURN n").is_ok());
}

#[test]
fn valid_inline_procedure_scope_clause_exports_star_bindings() {
    assert!(parse_and_validate("MATCH (n)-[:KNOWS]->(m) CALL (n) { RETURN * } RETURN n").is_ok());
}

#[test]
fn valid_inline_procedure_exports_union_result_bindings() {
    assert!(
        parse_and_validate("MATCH (n) CALL { RETURN n AS x UNION RETURN n AS x } RETURN x").is_ok()
    );
}

#[test]
fn invalid_inline_procedure_union_binding_mismatch() {
    let err = parse_and_validate("MATCH (n) CALL { RETURN n AS x UNION RETURN n AS y } RETURN x")
        .expect_err("expected inline procedure composite body binding mismatch to fail");
    assert!(
        err.to_string()
            .contains("composite query branches must expose the same result bindings"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_inline_procedure_return_star_union_binding_mismatch() {
    let err = parse_and_validate(
        "MATCH (n) CALL { RETURN 1 AS x } RETURN * \
             UNION \
             MATCH (n) CALL { RETURN 1 AS y } RETURN *",
    )
    .expect_err("expected RETURN * branches to include inline procedure exports");
    assert!(
        err.to_string()
            .contains("composite query branches must expose the same result bindings"),
        "unexpected error: {err}"
    );
}

#[test]
fn valid_nested_table_prefix_binding_in_scope() {
    assert!(parse_and_validate("TABLE t = { MATCH (n) RETURN n } RETURN t").is_ok());
}

#[test]
fn invalid_top_level_inline_procedure_scope_var_missing() {
    let err = parse_and_validate("CALL (n) { RETURN n }")
        .expect_err("expected top-level inline scope var to fail");
    assert!(
        err.to_string()
            .contains("inline procedure scope variable 'n' is not in scope"),
        "unexpected error: {err}"
    );
}

#[test]
fn valid_select_with_having() {
    assert!(
        parse_and_validate("SELECT n FROM myGraph MATCH (n) GROUP BY n HAVING COUNT(*) > 1")
            .is_ok()
    );
}

#[test]
fn valid_select_order_by_alias() {
    assert!(
        parse_and_validate("SELECT n.name AS name FROM myGraph MATCH (n) ORDER BY name").is_ok()
    );
}

#[test]
fn invalid_select_order_by_unknown_name() {
    let err = parse_and_validate("SELECT n.name AS name FROM myGraph MATCH (n) ORDER BY other")
        .expect_err("expected unknown ORDER BY name to fail");
    assert!(
        err.to_string().contains("variable 'other' is not in scope"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_select_group_by_ungrouped_non_aggregate_item() {
    let err = parse_and_validate("SELECT n.name, n.age FROM myGraph MATCH (n) GROUP BY n.name")
        .expect_err("expected ungrouped non-aggregate SELECT item to fail");
    assert!(
        err.to_string()
            .contains("SELECT item must be grouped or aggregated when GROUP BY is present"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_select_group_by_ungrouped_order_by_expr() {
    let err = parse_and_validate(
        "SELECT n.name, COUNT(*) FROM myGraph MATCH (n) GROUP BY n.name ORDER BY n.age",
    )
    .expect_err("expected ungrouped ORDER BY expression to fail");
    assert!(
        err.to_string()
            .contains("ORDER BY expression must be grouped or aggregated when GROUP BY is present"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_select_group_by_ungrouped_having_expr() {
    let err = parse_and_validate(
        "SELECT n.name, COUNT(*) FROM myGraph MATCH (n) GROUP BY n.name HAVING n.age > 1",
    )
    .expect_err("expected ungrouped HAVING expression to fail");
    assert!(
        err.to_string()
            .contains("HAVING expression must be grouped or aggregated when GROUP BY is present"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_return_duplicate_output_alias() {
    let err = parse_and_validate("MATCH (n) RETURN n AS x, n AS x")
        .expect_err("expected duplicate RETURN output alias to fail");
    assert!(
        err.to_string().contains("duplicate output name 'x'"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_select_duplicate_output_alias() {
    let err = parse_and_validate("SELECT 1 AS x, 2 AS x")
        .expect_err("expected duplicate SELECT output alias to fail");
    assert!(
        err.to_string().contains("duplicate output name 'x'"),
        "unexpected error: {err}"
    );
}

#[test]
fn valid_match_yield_alias_exports_binding() {
    assert!(parse_and_validate("MATCH (a)-[e]->(b) YIELD e AS edge RETURN edge").is_ok());
}

#[test]
fn invalid_match_yield_hides_non_yielded_binding() {
    let err = parse_and_validate("MATCH (a)-[e]->(b) YIELD e RETURN a")
        .expect_err("expected non-yielded binding to be hidden");
    assert!(
        err.to_string().contains("variable 'a' is not in scope"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_match_yield_unknown_binding() {
    let err = parse_and_validate("MATCH (a)-[e]->(b) YIELD z RETURN z")
        .expect_err("expected unknown YIELD binding to fail");
    assert!(
        err.to_string()
            .contains("MATCH YIELD variable 'z' is not in scope"),
        "unexpected error: {err}"
    );
}

#[test]
fn valid_select_nested_return_star_exports_bindings() {
    assert!(parse_and_validate("SELECT n FROM { MATCH (n) RETURN * }").is_ok());
}

#[test]
fn valid_select_graph_nested_return_star_exports_bindings() {
    assert!(parse_and_validate("SELECT n FROM myGraph { MATCH (n) RETURN * }").is_ok());
}

#[test]
fn valid_select_nested_union_exports_bindings() {
    assert!(
        parse_and_validate("SELECT n FROM { MATCH (n) RETURN n UNION MATCH (n) RETURN n }").is_ok()
    );
}

#[test]
fn invalid_select_graph_source_with_value_binding_name() {
    let err = parse_and_validate("VALUE g = 1 SELECT n FROM g MATCH (n)")
        .expect_err("expected non-graph binding used as SELECT graph source to fail");
    assert!(
        err.to_string()
            .contains("cannot be used as a graph reference"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_composite_query_binding_mismatch() {
    let err =
        parse_and_validate("SELECT n FROM { MATCH (n) RETURN n UNION MATCH (m) RETURN m AS x }")
            .expect_err("expected mismatched result bindings to fail");
    assert!(
        err.to_string()
            .contains("composite query branches must expose the same result bindings"),
        "unexpected error: {err}"
    );
}

#[test]
fn valid_create_graph_type_with_structured_body() {
    assert!(
            parse_and_validate(
                "CREATE GRAPH TYPE myType { NODE Person LABELS Person {name STRING} AS employee, NODE Manager LABELS Manager AS manager, DIRECTED EDGE WorksFor LABEL WorksFor CONNECTING (employee -> manager) }"
            )
            .is_ok()
        );
}

#[test]
fn invalid_create_graph_type_duplicate_property_name() {
    let err = parse_and_validate(
        "CREATE GRAPH TYPE myType { NODE Person LABELS Person {name STRING, name INT32} }",
    )
    .expect_err("expected duplicate graph type property to fail");
    assert!(
        err.to_string()
            .contains("duplicate graph type property 'name'"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_create_graph_type_duplicate_edge_name() {
    let err = parse_and_validate(
            "CREATE GRAPH TYPE myType { DIRECTED EDGE WorksFor LABEL WorksFor CONNECTING (employee -> manager), DIRECTED EDGE WorksFor LABEL ReportsTo CONNECTING (manager -> employee) }",
        )
        .expect_err("expected duplicate graph type edge name to fail");
    assert!(
        err.to_string()
            .contains("duplicate graph type edge name 'WorksFor'"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_create_graph_type_duplicate_node_alias() {
    let err = parse_and_validate(
            "CREATE GRAPH TYPE myType { NODE Person LABELS Person AS p, NODE Company LABELS Company AS p }",
        )
        .expect_err("expected duplicate graph type node alias to fail");
    assert!(
        err.to_string()
            .contains("duplicate graph type node alias 'p'"),
        "unexpected error: {err}"
    );
}

#[test]
fn valid_create_graph_with_inline_graph_type() {
    assert!(
            parse_and_validate(
                "CREATE GRAPH myGraph { NODE Person LABELS Person {name STRING} AS employee, NODE Manager LABELS Manager AS manager, DIRECTED EDGE WorksFor LABEL WorksFor CONNECTING (employee -> manager) }"
            )
            .is_ok()
        );
}

#[test]
fn invalid_create_graph_with_inline_graph_type_duplicate_property() {
    let err = parse_and_validate(
        "CREATE GRAPH myGraph { NODE Person LABELS Person {name STRING, name INT32} }",
    )
    .expect_err("expected duplicate inline graph type property to fail");
    assert!(
        err.to_string()
            .contains("duplicate graph type property 'name'"),
        "unexpected error: {err}"
    );
}

#[test]
fn valid_create_graph_type_edge_endpoint_matches_declared_node_refs() {
    assert!(
            parse_and_validate(
                "CREATE GRAPH TYPE myType { NODE Person LABELS Person AS employee, NODE Manager LABELS Manager AS manager, DIRECTED EDGE WorksFor LABEL WorksFor CONNECTING (employee -> manager) }"
            )
            .is_ok()
        );
}

#[test]
fn invalid_create_graph_type_edge_endpoint_unknown_node_ref() {
    let err = parse_and_validate(
            "CREATE GRAPH TYPE myType { NODE Person LABELS Person AS employee, DIRECTED EDGE WorksFor LABEL WorksFor CONNECTING (employee -> boss) }",
        )
        .expect_err("expected unknown endpoint reference to fail");
    assert!(
            err.to_string().contains(
                "graph type destination endpoint 'boss' does not match any node name, alias, or label in the same definition"
            ),
            "unexpected error: {err}"
        );
}

#[test]
fn invalid_create_graph_type_edge_endpoint_ambiguous_node_ref() {
    let err = parse_and_validate(
            "CREATE GRAPH TYPE myType { NODE Person LABELS Person, NODE Employee LABELS Person, DIRECTED EDGE WorksFor LABEL WorksFor CONNECTING (Person -> Person) }",
        )
        .expect_err("expected ambiguous endpoint reference to fail");
    assert!(
            err.to_string().contains(
                "graph type source endpoint 'Person' is ambiguous across multiple node names, aliases, or labels"
            ),
            "unexpected error: {err}"
        );
}

// ── Top-level DML statements (Statement::Set, Statement::Remove,
//    Statement::Delete, Statement::DropGraphType, Statement::Session) ──

#[test]
fn valid_top_level_set_statement() {
    // SET as a standalone (non-query) statement
    assert!(parse_and_validate("MATCH (n) SET n.x = 1 RETURN n").is_ok());
}

#[test]
fn valid_top_level_remove_statement() {
    assert!(parse_and_validate("MATCH (n) REMOVE n.x RETURN n").is_ok());
}

#[test]
fn valid_top_level_delete_statement() {
    assert!(parse_and_validate("MATCH (n) DELETE n").is_ok());
}

#[test]
fn valid_drop_graph_type_statement() {
    assert!(parse_and_validate("DROP GRAPH TYPE gt").is_ok());
}

// ── DELETE target variable not in scope ──

#[test]
fn invalid_delete_target_variable_not_in_scope() {
    let err = parse_and_validate("MATCH (n) DELETE x")
        .expect_err("expected unbound DELETE target to fail");
    assert!(
        err.to_string().contains("'x'") && err.to_string().contains("not in scope"),
        "unexpected error: {err}"
    );
}

// ── ORDER BY / LIMIT / OFFSET as linear query parts ──

#[test]
fn valid_order_by_in_linear_query() {
    assert!(parse_and_validate("MATCH (n) ORDER BY n.name RETURN n").is_ok());
}

#[test]
fn valid_limit_in_linear_query() {
    assert!(parse_and_validate("MATCH (n) LIMIT 10 RETURN n").is_ok());
}

#[test]
fn valid_offset_in_linear_query() {
    assert!(parse_and_validate("MATCH (n) OFFSET 5 RETURN n").is_ok());
}

#[test]
fn valid_order_by_limit_offset_combined() {
    assert!(parse_and_validate("MATCH (n) ORDER BY n.name LIMIT 10 OFFSET 5 RETURN n").is_ok());
}

// ── USE (focused) body variants ──

#[test]
fn valid_focused_match_body() {
    assert!(parse_and_validate("USE myGraph MATCH (n) RETURN n").is_ok());
}

#[test]
fn valid_focused_match_with_where() {
    assert!(parse_and_validate("USE myGraph MATCH (n) WHERE n.age > 30 RETURN n").is_ok());
}

#[test]
fn valid_focused_match_with_yield() {
    assert!(parse_and_validate("USE myGraph MATCH (a)-[e]->(b) YIELD e RETURN e").is_ok());
}

#[test]
fn invalid_focused_match_yield_unknown_binding() {
    let err = parse_and_validate("USE myGraph MATCH (a)-[e]->(b) YIELD z RETURN z")
        .expect_err("expected unknown focused YIELD binding to fail");
    assert!(
        err.to_string()
            .contains("MATCH YIELD variable 'z' is not in scope"),
        "unexpected error: {err}"
    );
}

#[test]
fn valid_focused_insert_body() {
    assert!(parse_and_validate("MATCH (n) USE myGraph INSERT (:Label) RETURN n").is_ok());
}

#[test]
fn valid_focused_set_body() {
    assert!(parse_and_validate("MATCH (n) USE myGraph SET n.x = 1 RETURN n").is_ok());
}

#[test]
fn valid_focused_remove_body() {
    assert!(parse_and_validate("MATCH (n) USE myGraph REMOVE n.x RETURN n").is_ok());
}

// ── Focused match with graph reference ──

#[cfg(feature = "cypher")]
#[test]
fn valid_match_on_graph_name() {
    // ON <graphName> goes right after MATCH, before the pattern.
    assert!(parse_and_validate("MATCH ON myGraph (n) RETURN n").is_ok());
}

// ── SELECT * with GROUP BY / HAVING / ORDER BY / LIMIT / OFFSET ──

#[test]
fn valid_select_star_with_group_by() {
    assert!(parse_and_validate("SELECT * FROM myGraph MATCH (n) GROUP BY n.name").is_ok());
}

#[test]
fn valid_select_star_with_having() {
    assert!(
        parse_and_validate("SELECT * FROM myGraph MATCH (n) GROUP BY n.name HAVING COUNT(*) > 1")
            .is_ok()
    );
}

#[test]
fn valid_select_star_with_order_by() {
    assert!(parse_and_validate("SELECT * FROM myGraph MATCH (n) ORDER BY n.name").is_ok());
}

#[test]
fn valid_select_star_with_limit() {
    assert!(parse_and_validate("SELECT * FROM myGraph MATCH (n) LIMIT 10").is_ok());
}

#[test]
fn valid_select_star_with_offset() {
    assert!(parse_and_validate("SELECT * FROM myGraph MATCH (n) OFFSET 5").is_ok());
}

#[test]
fn valid_select_star_with_all_clauses() {
    assert!(
            parse_and_validate(
                "SELECT * FROM myGraph MATCH (n) GROUP BY n.name HAVING COUNT(*) > 1 ORDER BY n.name LIMIT 10 OFFSET 5"
            )
            .is_ok()
        );
}

#[test]
fn invalid_select_star_group_by_ungrouped_having_expr() {
    let err =
        parse_and_validate("SELECT * FROM myGraph MATCH (n) GROUP BY n.name HAVING n.age > 1")
            .expect_err("expected ungrouped HAVING in SELECT * to fail");
    assert!(
        err.to_string()
            .contains("HAVING expression must be grouped or aggregated when GROUP BY is present"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_select_star_group_by_ungrouped_order_by_expr() {
    let err = parse_and_validate("SELECT * FROM myGraph MATCH (n) GROUP BY n.name ORDER BY n.age")
        .expect_err("expected ungrouped ORDER BY in SELECT * to fail");
    assert!(
        err.to_string()
            .contains("ORDER BY expression must be grouped or aggregated when GROUP BY is present"),
        "unexpected error: {err}"
    );
}

// ── SELECT items with LIMIT / OFFSET ──

#[test]
fn valid_select_items_with_limit() {
    assert!(parse_and_validate("SELECT n.name FROM myGraph MATCH (n) LIMIT 10").is_ok());
}

#[test]
fn valid_select_items_with_offset() {
    assert!(parse_and_validate("SELECT n.name FROM myGraph MATCH (n) OFFSET 5").is_ok());
}

// ── RETURN with GROUP BY / HAVING / ORDER BY ungrouped ──

#[test]
fn invalid_return_order_by_ungrouped() {
    let err = parse_and_validate("MATCH (n) RETURN n.name GROUP BY n.name ORDER BY n.age")
        .expect_err("expected ungrouped ORDER BY in RETURN to fail");
    assert!(
        err.to_string()
            .contains("ORDER BY expression must be grouped or aggregated when GROUP BY is present"),
        "unexpected error: {err}"
    );
}

#[test]
fn valid_return_having() {
    assert!(
        parse_and_validate("MATCH (n) RETURN n.name GROUP BY n.name HAVING COUNT(*) > 1").is_ok()
    );
}

#[test]
fn valid_return_group_by_limit_offset() {
    assert!(
        parse_and_validate("MATCH (n) RETURN n.name GROUP BY n.name LIMIT 10 OFFSET 5").is_ok()
    );
}

// ── NEXT YIELD alias uniqueness ──

#[test]
fn valid_next_yield() {
    assert!(
        parse_and_validate("MATCH (a)-[e]->(b) RETURN a NEXT YIELD a MATCH (a)-[e2]->(c) RETURN c")
            .is_ok()
    );
}

#[test]
fn valid_next_yield_alias_propagates_scope() {
    assert!(parse_and_validate("MATCH (n) RETURN n AS x NEXT YIELD x MATCH (m) RETURN x").is_ok());
}

#[test]
fn valid_next_yield_preserves_graph_scope() {
    assert!(
        parse_and_validate("GRAPH g = myGraph RETURN g NEXT YIELD g USE g MATCH (n) RETURN n")
            .is_ok()
    );
}

#[test]
fn invalid_match_yield_hides_graph_bindings() {
    let err = parse_and_validate(
        "GRAPH g = myGraph MATCH (n) YIELD n NEXT VALUE g = 1 USE g MATCH (m) RETURN m",
    )
    .expect_err("expected hidden graph binding after MATCH YIELD to fail");
    assert!(
        err.to_string()
            .contains("cannot be used as a graph reference"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_next_yield_unknown_binding() {
    let err = parse_and_validate("MATCH (n) RETURN n NEXT YIELD x MATCH (m) RETURN m")
        .expect_err("expected unknown NEXT YIELD binding to fail");
    assert!(
        err.to_string()
            .contains("NEXT YIELD variable 'x' is not in scope"),
        "unexpected error: {err}"
    );
}

// ── FOR statement with ordinality ──

#[test]
fn valid_for_with_ordinality() {
    assert!(
        parse_and_validate("MATCH (n) FOR x IN [1, 2, 3] WITH ORDINALITY i RETURN x, i").is_ok()
    );
}

#[test]
fn valid_for_basic() {
    assert!(parse_and_validate("MATCH (n) FOR x IN [1, 2, 3] RETURN x").is_ok());
}

// ── CALL procedure basic ──

#[test]
fn valid_call_procedure_yield() {
    assert!(parse_and_validate("CALL myproc() YIELD x RETURN x").is_ok());
}

// ── Inline CALL duplicate scope variable ──

#[test]
fn invalid_inline_call_duplicate_scope_var() {
    let err = parse_and_validate("MATCH (n) CALL (n, n) { RETURN n } RETURN n")
        .expect_err("expected duplicate inline scope var to fail");
    assert!(
        err.to_string()
            .contains("inline CALL: duplicate scope variable 'n'"),
        "unexpected error: {err}"
    );
}

// ── START TRANSACTION contradictory access modes ──

#[test]
fn invalid_start_transaction_contradictory_modes() {
    let err = parse_and_validate("START TRANSACTION READ ONLY, READ WRITE MATCH (n) RETURN n")
        .expect_err("expected contradictory transaction modes to fail");
    assert!(
        err.to_string().contains("contradictory access modes"),
        "unexpected error: {err}"
    );
}

// ── YIELD alias uniqueness ──

#[test]
fn invalid_match_yield_duplicate_alias() {
    let err = parse_and_validate("MATCH (a)-[e]->(b) YIELD a, a RETURN a")
        .expect_err("expected duplicate MATCH YIELD alias to fail");
    assert!(
        err.to_string().contains("duplicate output name 'a'"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_call_yield_duplicate_alias() {
    let err = parse_and_validate("CALL myproc() YIELD x, x RETURN x")
        .expect_err("expected duplicate CALL YIELD alias to fail");
    assert!(
        err.to_string().contains("duplicate output name 'x'"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_match_yield_hides_previous_bindings() {
    let err = parse_and_validate("MATCH (p) MATCH (a) YIELD a AS x RETURN p")
        .expect_err("expected hidden binding after MATCH YIELD to fail");
    assert!(err.to_string().contains("'p'"), "unexpected error: {err}");
}

#[test]
fn invalid_focused_match_yield_hides_previous_bindings() {
    let err = parse_and_validate("MATCH (p) USE myGraph MATCH (a) YIELD a AS x RETURN p")
        .expect_err("expected hidden binding after focused MATCH YIELD to fail");
    assert!(err.to_string().contains("'p'"), "unexpected error: {err}");
}

#[test]
fn invalid_select_prefix_unbound_yield_is_still_checked() {
    let err = parse_and_validate("MATCH (n) YIELD z SELECT * FROM myGraph MATCH (m)")
        .expect_err("expected invalid MATCH YIELD before SELECT to fail");
    assert!(
        err.to_string()
            .contains("MATCH YIELD variable 'z' is not in scope"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_select_prefix_unbound_where_is_still_checked() {
    let err = parse_and_validate("MATCH (n) WHERE z = 1 SELECT * FROM myGraph MATCH (m)")
        .expect_err("expected invalid MATCH WHERE before SELECT to fail");
    assert!(err.to_string().contains("'z'"), "unexpected error: {err}");
}

// ── Composite query binding mismatch (top-level UNION) ──

#[test]
fn invalid_union_binding_mismatch() {
    let err = parse_and_validate("MATCH (n) RETURN n UNION MATCH (m) RETURN m AS x")
        .expect_err("expected union binding mismatch to fail");
    assert!(
        err.to_string()
            .contains("composite query branches must expose the same result bindings"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_union_named_result_order_mismatch() {
    let err = parse_and_validate(
        "MATCH (n),(m) RETURN n AS x, m AS y UNION MATCH (a),(b) RETURN b AS y, a AS x",
    )
    .expect_err("expected named union column order mismatch to fail");
    assert!(
        err.to_string()
            .contains("composite query branches must expose the same result bindings"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_union_unnamed_result_column_count_mismatch() {
    let err = parse_and_validate("MATCH (n) RETURN 1 UNION MATCH (m) RETURN 2, 3")
        .expect_err("expected unnamed union column-count mismatch to fail");
    assert!(
        err.to_string()
            .contains("composite query branches must expose the same result bindings"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_union_star_result_order_mismatch() {
    let err = parse_and_validate("MATCH (a), (b) RETURN * UNION MATCH (b), (a) RETURN *")
        .expect_err("expected star union column order mismatch to fail");
    assert!(
        err.to_string()
            .contains("composite query branches must expose the same result bindings"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_next_yield_graph_binding_must_exist_in_all_union_branches() {
    let err = parse_and_validate(
            "START TRANSACTION GRAPH g = myGraph RETURN g UNION RETURN 1 AS g NEXT YIELD g USE g MATCH (n) RETURN n COMMIT",
        )
        .expect_err("expected mixed graph/value union binding to fail");
    assert!(
        err.to_string()
            .contains("composite query branches must expose the same result bindings"),
        "unexpected error: {err}"
    );
}

#[test]
fn invalid_union_mixed_named_and_unnamed_result_column_count_mismatch() {
    let err = parse_and_validate("MATCH (n) RETURN 1 AS x, 2 UNION MATCH (m) RETURN 3 AS x")
        .expect_err("expected mixed named/unnamed union mismatch to fail");
    assert!(
        err.to_string()
            .contains("composite query branches must expose the same result bindings"),
        "unexpected error: {err}"
    );
}

// ── CREATE GRAPH: IF NOT EXISTS and OR REPLACE mutually exclusive ──

#[test]
fn invalid_create_graph_if_not_exists_and_or_replace() {
    // We construct this via the AST since the parser won't produce both.
    let create = CreateGraphStatement {
        span: Span::DUMMY,
        property_keyword: false,
        if_not_exists: true,
        or_replace: true,
        name: ObjectName {
            parts: vec!["g".into()],
        },
        graph_type: Some(GraphTypeSpec::Any {
            property_keyword: false,
            graph_keyword: false,
        }),
        copy_of: None,
    };
    let result = validate_create_graph(&create);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("IF NOT EXISTS and OR REPLACE are mutually exclusive"),
    );
}

// ── CREATE GRAPH TYPE: IF NOT EXISTS and OR REPLACE mutually exclusive ──

#[test]
fn invalid_create_graph_type_if_not_exists_and_or_replace() {
    let create = CreateGraphTypeStatement {
        span: Span::DUMMY,
        property_keyword: false,
        if_not_exists: true,
        or_replace: true,
        name: ObjectName {
            parts: vec!["gt".into()],
        },
        definition: GraphTypeDefinition {
            span: Span::DUMMY,
            elements: vec![],
        },
        as_keyword: false,
        copy_of: None,
    };
    let result = validate_create_graph_type(&create);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("IF NOT EXISTS and OR REPLACE are mutually exclusive"),
    );
}

// ── Focused body: CallProcedure with YIELD collects bindings ──

#[test]
fn valid_focused_call_procedure_yield_exports_bindings() {
    assert!(parse_and_validate("MATCH (n) CALL myproc() YIELD x RETURN n, x").is_ok());
}

#[test]
fn invalid_focused_call_procedure_unbound_arg() {
    let err = parse_and_validate("MATCH (n) USE myGraph CALL myproc(x) YIELD z RETURN z")
        .expect_err("expected unbound arg in focused CALL");
    assert!(err.to_string().contains("'x'"), "unexpected error: {err}");
}

// ── SELECT with nested subquery ──

#[test]
fn valid_select_from_nested_query() {
    assert!(parse_and_validate("SELECT n FROM { MATCH (n) RETURN n }").is_ok());
}

#[test]
fn valid_select_from_graph_nested_query() {
    assert!(parse_and_validate("SELECT n FROM myGraph { MATCH (n) RETURN n }").is_ok());
}

// ── INSERT into graph via INSERT statement ──

#[test]
fn valid_standalone_insert() {
    assert!(parse_and_validate("INSERT (:Person {name: 'Alice'})").is_ok());
}

// ── Duplicate graph type node name ──

#[test]
fn invalid_create_graph_type_duplicate_node_name() {
    let err = parse_and_validate(
        "CREATE GRAPH TYPE myType { NODE Person LABELS Person, NODE Person LABELS Employee }",
    )
    .expect_err("expected duplicate node name to fail");
    assert!(
        err.to_string()
            .contains("duplicate graph type node name 'Person'"),
        "unexpected error: {err}"
    );
}

// ── Expr validation: various expression types ──

#[cfg(feature = "cypher")]
#[test]
fn valid_list_slice_expr() {
    assert!(parse_and_validate("MATCH (n) LET x = [1,2,3] RETURN x[0..2]").is_ok());
}

#[test]
fn valid_case_simple_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN CASE n.x WHEN 1 THEN 'a' ELSE 'b' END").is_ok());
}

#[test]
fn valid_case_searched_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN CASE WHEN n.x > 1 THEN 'a' ELSE 'b' END").is_ok());
}

#[test]
fn valid_exists_pattern_with_where() {
    assert!(
        parse_and_validate("MATCH (n) WHERE EXISTS { (n)-[e]->(m) WHERE m.age > 30 } RETURN n")
            .is_ok()
    );
}

#[test]
fn valid_let_in_expression() {
    assert!(parse_and_validate("MATCH (n) RETURN LET x = n.age IN x + 1 END").is_ok());
}

#[test]
fn valid_property_exists_expr() {
    assert!(parse_and_validate("MATCH (n) WHERE n.name IS NOT NULL RETURN n").is_ok());
}

#[test]
fn valid_function_call_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN abs(n.x)").is_ok());
}

#[test]
fn valid_aggregate_count() {
    assert!(parse_and_validate("MATCH (n) RETURN COUNT(n.x)").is_ok());
}

#[test]
fn valid_concat_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN n.first || n.last").is_ok());
}

#[test]
fn valid_record_literal() {
    assert!(parse_and_validate("MATCH (n) RETURN {name: n.name, age: n.age}").is_ok());
}

#[test]
fn valid_list_literal() {
    assert!(parse_and_validate("MATCH (n) RETURN [n.x, n.y, n.z]").is_ok());
}

#[test]
fn valid_coalesce_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN COALESCE(n.name, 'unknown')").is_ok());
}

// ── Multiset alternation / pattern union in MATCH ──

#[test]
fn valid_multiset_alternation() {
    assert!(parse_and_validate("MATCH (a)(()-[:A]->() | ()-[:B]->())(b) RETURN a, b").is_ok());
}

// ── SELECT body Star extends scope ──

#[test]
fn valid_select_star_extends_scope() {
    assert!(parse_and_validate("SELECT * FROM myGraph MATCH (n)").is_ok());
}

// ── Session command validation ──

#[test]
fn valid_session_reset_all() {
    assert!(parse_and_validate("SESSION RESET ALL PARAMETERS MATCH (n) RETURN n").is_ok());
}

#[test]
fn valid_session_close() {
    assert!(parse_and_validate("SESSION CLOSE").is_ok());
}

// ── SELECT with GROUP BY on items ──

#[test]
fn valid_select_items_with_group_by() {
    assert!(parse_and_validate("SELECT n.name FROM myGraph MATCH (n) GROUP BY n.name").is_ok());
}

#[test]
fn valid_select_items_with_having_and_group_by() {
    assert!(
        parse_and_validate(
            "SELECT n.name, COUNT(*) FROM myGraph MATCH (n) GROUP BY n.name HAVING COUNT(*) > 1"
        )
        .is_ok()
    );
}

// ── RETURN with GROUP BY validates group_by expressions ──

#[test]
fn valid_return_with_group_by() {
    assert!(parse_and_validate("MATCH (n) RETURN n.name GROUP BY n.name").is_ok());
}

// ── FINISH result statement ──

#[test]
fn valid_finish_statement() {
    assert!(parse_and_validate("MATCH (n) FINISH").is_ok());
}

// ── Graph type edge endpoint missing reference ──

#[test]
fn invalid_create_graph_type_edge_endpoint_missing_ref() {
    // Edge without any endpoint type/label reference; construct via AST.
    let def = GraphTypeDefinition {
        span: Span::DUMMY,
        elements: vec![
            GraphTypeElement::Node(NodeTypeDef {
                span: Span::DUMMY,
                keyword: Keyword::new("NODE"),
                name: Some("Person".into()),
                alias: None,
                label_set: None,
                properties: vec![],
            }),
            GraphTypeElement::Edge(EdgeTypeDef {
                span: Span::DUMMY,
                keyword: Keyword::new("EDGE"),
                name: Some("Knows".into()),
                label_set: None,
                direction: crate::types::EdgeDirection::PointingRight,
                source: EdgeEndpoint {
                    span: Span::DUMMY,
                    type_name: None,
                    label: None,
                },
                destination: EdgeEndpoint {
                    span: Span::DUMMY,
                    type_name: Some("Person".into()),
                    label: None,
                },
                properties: vec![],
            }),
        ],
    };
    let result = validate_graph_type_definition(&def);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("endpoint is missing a node reference"),
    );
}

#[test]
fn invalid_graph_type_conflicting_edge_label_directedness() {
    let label_r = KeyLabelSet {
        span: Span::DUMMY,
        label_keyword_plural: false,
        labels: vec!["R".into()],
    };
    let def = GraphTypeDefinition {
        span: Span::DUMMY,
        elements: vec![
            GraphTypeElement::Node(NodeTypeDef {
                span: Span::DUMMY,
                keyword: Keyword::new("NODE"),
                name: Some("A".into()),
                alias: None,
                label_set: None,
                properties: vec![],
            }),
            GraphTypeElement::Node(NodeTypeDef {
                span: Span::DUMMY,
                keyword: Keyword::new("NODE"),
                name: Some("B".into()),
                alias: None,
                label_set: None,
                properties: vec![],
            }),
            GraphTypeElement::Edge(EdgeTypeDef {
                span: Span::DUMMY,
                keyword: Keyword::new("EDGE"),
                name: Some("E1".into()),
                direction: crate::types::EdgeDirection::PointingRight,
                source: EdgeEndpoint {
                    span: Span::DUMMY,
                    label: None,
                    type_name: Some("A".into()),
                },
                destination: EdgeEndpoint {
                    span: Span::DUMMY,
                    label: None,
                    type_name: Some("B".into()),
                },
                label_set: Some(label_r.clone()),
                properties: vec![],
            }),
            GraphTypeElement::Edge(EdgeTypeDef {
                span: Span::DUMMY,
                keyword: Keyword::new("EDGE"),
                name: Some("E2".into()),
                direction: crate::types::EdgeDirection::Undirected,
                source: EdgeEndpoint {
                    span: Span::DUMMY,
                    label: None,
                    type_name: Some("A".into()),
                },
                destination: EdgeEndpoint {
                    span: Span::DUMMY,
                    label: None,
                    type_name: Some("B".into()),
                },
                label_set: Some(label_r),
                properties: vec![],
            }),
        ],
    };
    let result = validate_graph_type_definition(&def);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("conflicting directedness"),
    );
}

// ── Return with HAVING expression ──

#[test]
fn valid_return_having_with_aggregate() {
    assert!(
        parse_and_validate("MATCH (n) RETURN n.name, COUNT(*) GROUP BY n.name HAVING COUNT(*) > 1")
            .is_ok()
    );
}

// ── String predicate expressions ──

#[cfg(feature = "cypher")]
#[test]
fn valid_starts_with_expr() {
    assert!(parse_and_validate("MATCH (n) WHERE n.name STARTS WITH 'A' RETURN n").is_ok());
}

// ── RETURN with LIMIT and OFFSET ──

#[test]
fn valid_return_limit() {
    assert!(parse_and_validate("MATCH (n) RETURN n LIMIT 10").is_ok());
}

#[test]
fn valid_return_offset() {
    assert!(parse_and_validate("MATCH (n) RETURN n OFFSET 5").is_ok());
}

// ── INSERT with graph name ──

#[cfg(feature = "cypher")]
#[test]
fn valid_insert_into_graph() {
    assert!(parse_and_validate("INSERT INTO myGraph (:Person {name: 'Alice'})").is_ok());
}

// ════════════════════════════════════════════════════════════════════════
// Additional tests targeting uncovered code paths
// ════════════════════════════════════════════════════════════════════════

// ── SESSION SET parameter validation (lines 44-56) ──

#[test]
fn valid_session_set_schema() {
    assert!(parse_and_validate("SESSION SET SCHEMA /mySchema MATCH (n) RETURN n").is_ok());
}

#[test]
fn valid_session_set_graph() {
    assert!(parse_and_validate("SESSION SET GRAPH myGraph MATCH (n) RETURN n").is_ok());
}

#[test]
fn valid_session_set_time_zone() {
    assert!(parse_and_validate("SESSION SET TIME ZONE 'UTC' MATCH (n) RETURN n").is_ok());
}

#[test]
fn valid_session_set_value_parameter() {
    assert!(parse_and_validate("SESSION SET VALUE $x = 42 MATCH (n) RETURN n").is_ok());
}

#[test]
fn valid_session_set_graph_parameter() {
    assert!(parse_and_validate("SESSION SET GRAPH $g = myGraph MATCH (n) RETURN n").is_ok());
}

#[test]
fn valid_session_set_table_parameter() {
    assert!(parse_and_validate("SESSION SET TABLE $t = myTable MATCH (n) RETURN n").is_ok());
}

#[test]
fn invalid_session_set_empty_parameter_name() {
    // Direct AST construction since parser won't produce empty param name.
    let cmd = SessionSetCommand::Parameter {
        if_not_exists: false,
        name: String::new(),
        typed_prefix: TypedPrefix::None,
        type_annotation: None,
        value: Box::new(Expr::new(ExprKind::Literal(crate::value::Value::Int32(1)))),
    };
    assert!(validate_session_set(&cmd).is_err());
}

#[test]
fn invalid_session_set_empty_graph_parameter_name() {
    let cmd = SessionSetCommand::GraphParameter {
        property_keyword: false,
        if_not_exists: false,
        name: String::new(),
        typed_prefix: TypedPrefix::None,
        type_annotation: None,
        value: Box::new(Expr::new(ExprKind::Literal(crate::value::Value::Int32(1)))),
    };
    let err = validate_session_set(&cmd).unwrap_err();
    assert!(err.to_string().contains("parameter name must not be empty"));
}

#[test]
fn invalid_session_set_empty_binding_table_parameter_name() {
    let cmd = SessionSetCommand::BindingTableParameter {
        binding_keyword: false,
        if_not_exists: false,
        name: String::new(),
        typed_prefix: TypedPrefix::None,
        type_annotation: None,
        value: Box::new(Expr::new(ExprKind::Literal(crate::value::Value::Int32(1)))),
    };
    let err = validate_session_set(&cmd).unwrap_err();
    assert!(err.to_string().contains("parameter name must not be empty"));
}

#[test]
fn valid_session_set_schema_no_validation_error() {
    // The Schema variant hits the catch-all `_ => Ok(())` in validate_session_set.
    let cmd = SessionSetCommand::Schema(ObjectName {
        parts: vec!["mySchema".into()],
    });
    assert!(validate_session_set(&cmd).is_ok());
}

// ── CREATE SCHEMA / DROP SCHEMA (lines 254-296) ──

#[test]
fn valid_create_schema() {
    assert!(parse_and_validate("CREATE SCHEMA /mySchema").is_ok());
}

#[test]
fn valid_drop_schema() {
    assert!(parse_and_validate("DROP SCHEMA /mySchema").is_ok());
}

#[test]
fn valid_drop_graph() {
    assert!(parse_and_validate("DROP GRAPH myGraph").is_ok());
}

#[test]
fn invalid_create_schema_empty_name() {
    let create = CreateSchemaStatement {
        span: Span::DUMMY,
        if_not_exists: false,
        name: ObjectName { parts: vec![] },
    };
    let err = validate_create_schema(&create).unwrap_err();
    assert!(
        err.to_string()
            .contains("CREATE SCHEMA requires a non-empty name")
    );
}

#[test]
fn invalid_drop_name_empty() {
    let name = ObjectName { parts: vec![] };
    let err = validate_drop_name(&name, "DROP SCHEMA").unwrap_err();
    assert!(
        err.to_string()
            .contains("DROP SCHEMA requires a non-empty name")
    );
}

#[test]
fn invalid_create_graph_empty_name() {
    let create = CreateGraphStatement {
        span: Span::DUMMY,
        property_keyword: false,
        if_not_exists: false,
        or_replace: false,
        name: ObjectName { parts: vec![] },
        graph_type: None,
        copy_of: None,
    };
    let err = validate_create_graph(&create).unwrap_err();
    assert!(
        err.to_string()
            .contains("CREATE GRAPH requires a non-empty name")
    );
}

#[test]
fn invalid_create_graph_type_empty_name() {
    let create = CreateGraphTypeStatement {
        span: Span::DUMMY,
        property_keyword: false,
        if_not_exists: false,
        or_replace: false,
        name: ObjectName { parts: vec![] },
        definition: GraphTypeDefinition {
            span: Span::DUMMY,
            elements: vec![],
        },
        as_keyword: false,
        copy_of: None,
    };
    let err = validate_create_graph_type(&create).unwrap_err();
    assert!(
        err.to_string()
            .contains("CREATE GRAPH TYPE requires a non-empty name")
    );
}

// ── Top-level DML Statement dispatch (lines 108-133) ──

#[test]
fn valid_top_level_standalone_set() {
    // Tests Statement::Set dispatch (line 126).
    assert!(parse_and_validate("MATCH (n) SET n.x = 1").is_ok());
}

#[test]
fn valid_top_level_standalone_delete() {
    // Tests Statement::Delete dispatch (line 128).
    assert!(parse_and_validate("MATCH (n) DELETE n").is_ok());
}

// ── TRIM expression with trim_char (line 1355-1360) ──

#[test]
fn valid_trim_with_char() {
    assert!(parse_and_validate("MATCH (n) RETURN TRIM(LEADING ' ' FROM n.name)").is_ok());
}

#[test]
fn valid_trim_without_char() {
    assert!(parse_and_validate("MATCH (n) RETURN TRIM(n.name)").is_ok());
}

// ── FOLD STRING expression (lines 1395-1401) ──

#[test]
fn valid_normalize_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN NORMALIZE(n.name, NFC)").is_ok());
}

// ── Various binary expressions (lines 1301-1319) ──

#[test]
fn valid_nullif_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN NULLIF(n.x, 0)").is_ok());
}

#[test]
fn valid_mod_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN mod(n.x, 3)").is_ok());
}

#[test]
fn valid_power_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN POWER(n.x, 2)").is_ok());
}

#[test]
fn valid_log_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN LOG(n.x, 10)").is_ok());
}

// ── Unary math functions (lines 1338-1353) ──

#[test]
fn valid_sqrt_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN SQRT(n.x)").is_ok());
}

#[test]
fn valid_exp_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN EXP(n.x)").is_ok());
}

#[test]
fn valid_ln_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN LN(n.x)").is_ok());
}

#[test]
fn valid_log10_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN LOG10(n.x)").is_ok());
}

#[test]
fn valid_sin_cos_tan_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN SIN(n.x)").is_ok());
    assert!(parse_and_validate("MATCH (n) RETURN COS(n.x)").is_ok());
    assert!(parse_and_validate("MATCH (n) RETURN TAN(n.x)").is_ok());
}

#[test]
fn valid_asin_acos_atan_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN ASIN(n.x)").is_ok());
    assert!(parse_and_validate("MATCH (n) RETURN ACOS(n.x)").is_ok());
    assert!(parse_and_validate("MATCH (n) RETURN ATAN(n.x)").is_ok());
}

#[test]
fn valid_degrees_radians_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN DEGREES(n.x)").is_ok());
    assert!(parse_and_validate("MATCH (n) RETURN RADIANS(n.x)").is_ok());
}

#[test]
fn valid_floor_ceil_abs_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN FLOOR(n.x)").is_ok());
    assert!(parse_and_validate("MATCH (n) RETURN CEIL(n.x)").is_ok());
    assert!(parse_and_validate("MATCH (n) RETURN ABS(n.x)").is_ok());
}

// ── Char/Byte length, Cardinality (lines 1335-1337) ──

#[test]
fn valid_char_length_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN CHAR_LENGTH(n.name)").is_ok());
}

#[test]
fn valid_byte_length_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN BYTE_LENGTH(n.name)").is_ok());
}

#[test]
fn valid_cardinality_expr() {
    assert!(parse_and_validate("MATCH (n) LET x = [1, 2] RETURN CARDINALITY(x)").is_ok());
}

// ── UPPER / LOWER (lines 1333-1334) ──

#[test]
fn valid_upper_lower_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN UPPER(n.name)").is_ok());
    assert!(parse_and_validate("MATCH (n) RETURN LOWER(n.name)").is_ok());
}

// ── Cast expression (line 1331) ──

#[test]
fn valid_cast_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN CAST(n.x AS STRING)").is_ok());
}

// ── IS NULL / IS NOT NULL (lines 1324-1325) ──

#[test]
fn valid_is_null_expr() {
    assert!(parse_and_validate("MATCH (n) WHERE n.x IS NULL RETURN n").is_ok());
    assert!(parse_and_validate("MATCH (n) WHERE n.x IS NOT NULL RETURN n").is_ok());
}

// ── NOT expression (line 1322) ──

#[test]
fn valid_not_expr() {
    assert!(parse_and_validate("MATCH (n) WHERE NOT n.active RETURN n").is_ok());
}

// ── Paren expression (line 1321) ──

#[test]
fn valid_paren_expr() {
    assert!(parse_and_validate("MATCH (n) WHERE (n.x > 1) RETURN n").is_ok());
}

// ── XOR expression (line 1304) ──

#[test]
fn valid_xor_expr() {
    assert!(parse_and_validate("MATCH (n) WHERE n.a XOR n.b RETURN n").is_ok());
}

// ── OR expression (line 1303) ──

#[test]
fn valid_or_expr() {
    assert!(parse_and_validate("MATCH (n) WHERE n.a OR n.b RETURN n").is_ok());
}

// ── Aggregate with filter and order_by (lines 1495-1517) ──

#[test]
fn valid_aggregate_sum_with_distinct() {
    assert!(parse_and_validate("MATCH (n) RETURN SUM(DISTINCT n.x)").is_ok());
}

#[test]
fn valid_aggregate_collect_list() {
    assert!(parse_and_validate("MATCH (n) RETURN COLLECT_LIST(n.name)").is_ok());
}

// ── Record literal validation (line 1440-1444) ──

#[test]
fn invalid_record_literal_unbound_var() {
    let err = parse_and_validate("MATCH (n) RETURN {name: x}")
        .expect_err("expected unbound var in record literal");
    assert!(err.to_string().contains("'x'"));
}

// ── List literal unbound var (line 1417-1438) ──

#[test]
fn invalid_list_literal_unbound_var() {
    let err =
        parse_and_validate("MATCH (n) RETURN [x, n.y]").expect_err("expected unbound var in list");
    assert!(err.to_string().contains("'x'"));
}

// ── CASE expression with unbound var ──

#[test]
fn invalid_case_simple_unbound_operand() {
    let err = parse_and_validate("MATCH (n) RETURN CASE x WHEN 1 THEN 'a' END")
        .expect_err("expected unbound var in CASE");
    assert!(err.to_string().contains("'x'"));
}

#[test]
fn invalid_case_searched_unbound_condition() {
    let err = parse_and_validate("MATCH (n) RETURN CASE WHEN x > 1 THEN 'a' END")
        .expect_err("expected unbound var in CASE WHEN");
    assert!(err.to_string().contains("'x'"));
}

// ── EXISTS subquery (line 1526-1529) ──

#[test]
fn valid_value_subquery() {
    assert!(parse_and_validate("MATCH (n) RETURN VALUE { MATCH (m) RETURN COUNT(*) }").is_ok());
}

// ── String predicate unbound (lines 1412-1415) ──

#[cfg(feature = "cypher")]
#[test]
fn invalid_starts_with_unbound_var() {
    let err = parse_and_validate("MATCH (n) WHERE x STARTS WITH 'A' RETURN n")
        .expect_err("expected unbound var in STARTS WITH");
    assert!(err.to_string().contains("'x'"));
}

// ── ElementId, PathLength, Elements expressions ──

#[test]
fn valid_element_id_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN ELEMENT_ID(n)").is_ok());
}

// ── PROPERTY EXISTS expression (line 1546) ──

#[test]
fn valid_property_exists_check() {
    assert!(parse_and_validate("MATCH (n) WHERE n.name IS NOT NULL RETURN n").is_ok());
}

// ── LetIn expression (line 1540-1544) ──

#[test]
fn invalid_let_in_unbound_in_binding() {
    let err = parse_and_validate("MATCH (n) RETURN LET y = x IN y + 1 END")
        .expect_err("expected unbound var in LET binding");
    assert!(err.to_string().contains("'x'"));
}

// ── Inline procedure with omitted scope clause (line 492-493) ──

#[test]
fn valid_inline_procedure_omitted_scope_clause() {
    // Omitted scope clause means entire outer scope is passed through.
    assert!(parse_and_validate("MATCH (n) CALL { RETURN n } RETURN n").is_ok());
}

// ── NEXT YIELD duplicate alias ──

#[test]
fn invalid_next_yield_duplicate_alias() {
    let err = parse_and_validate(
        "MATCH (a)-[e]->(b) RETURN a, b NEXT YIELD a, a MATCH (a)-[e2]->(c) RETURN c",
    )
    .expect_err("expected duplicate NEXT YIELD alias to fail");
    assert!(
        err.to_string().contains("duplicate output name 'a'"),
        "unexpected error: {err}"
    );
}

// ── Validate transaction body with NEXT statement ──

#[test]
fn valid_transaction_with_next() {
    assert!(
        parse_and_validate(
            "START TRANSACTION READ ONLY MATCH (n) RETURN n NEXT MATCH (n) RETURN n COMMIT"
        )
        .is_ok()
    );
}

#[test]
fn valid_start_transaction_read_only() {
    assert!(parse_and_validate("START TRANSACTION READ ONLY MATCH (n) RETURN n COMMIT").is_ok());
}

#[test]
fn valid_start_transaction_read_write() {
    assert!(parse_and_validate("START TRANSACTION READ WRITE MATCH (n) RETURN n COMMIT").is_ok());
}

// ── INSERT empty patterns (direct AST, line 1209-1211) ──

#[test]
fn invalid_insert_empty_patterns() {
    let ins = InsertStatement {
        span: Span::DUMMY,
        graph_name: None,
        patterns: vec![],
    };
    let err = validate_insert(&ins).unwrap_err();
    assert!(
        err.to_string()
            .contains("INSERT must have at least one pattern")
    );
}

// ── LET empty bindings (direct AST, line 1263-1265) ──

#[test]
fn invalid_let_empty_bindings() {
    let err =
        validate_let(&[], &mut RapidHashSet::default(), &RapidHashSet::default()).unwrap_err();
    assert!(
        err.to_string()
            .contains("LET must have at least one binding")
    );
}

// ── SELECT source: GraphMatchList path (lines 716-724) ──

#[test]
fn valid_select_from_graph_match_list() {
    assert!(parse_and_validate("SELECT n FROM myGraph MATCH (n)").is_ok());
}

// ── Focused body with graph reference validation (line 416-417) ──

#[cfg(feature = "cypher")]
#[test]
fn valid_focused_match_with_inner_graph_ref() {
    assert!(parse_and_validate("USE myGraph MATCH ON otherGraph (n) RETURN n").is_ok());
}

// ── Compare expression validation ──

#[test]
fn valid_compare_expr() {
    assert!(parse_and_validate("MATCH (n) WHERE n.x >= n.y RETURN n").is_ok());
}

// ── IS LABELED expression ──

#[test]
fn valid_is_labeled_expr() {
    assert!(parse_and_validate("MATCH (n) WHERE n IS LABELED Person RETURN n").is_ok());
}

// ── IS TRUTH expression ──

#[test]
fn valid_is_truth_expr() {
    assert!(parse_and_validate("MATCH (n) WHERE n.active IS TRUE RETURN n").is_ok());
}

// ── IS TYPED expression ──

#[test]
fn valid_is_typed_expr() {
    assert!(
        parse_and_validate("MATCH (n) WHERE n.x IS TYPED STRING RETURN n").is_ok()
            || parse_and_validate("MATCH (n) WHERE n.x :: STRING RETURN n").is_ok()
    );
}

// ── IS NORMALIZED expression ──

#[test]
fn valid_is_normalized_expr() {
    assert!(parse_and_validate("MATCH (n) WHERE n.name IS NORMALIZED RETURN n").is_ok());
}

// ── UnaryOp expression ──

#[test]
fn valid_unary_minus_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN -n.x").is_ok());
}

// ── BinaryOp expression (arithmetic) ──

#[test]
fn valid_binary_arithmetic_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN n.x + n.y").is_ok());
    assert!(parse_and_validate("MATCH (n) RETURN n.x - n.y").is_ok());
    assert!(parse_and_validate("MATCH (n) RETURN n.x * n.y").is_ok());
    assert!(parse_and_validate("MATCH (n) RETURN n.x / n.y").is_ok());
}

// ── LEFT / RIGHT string functions (lines 1312-1313) ──

#[test]
fn valid_left_right_string_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN LEFT(n.name, 3)").is_ok());
    assert!(parse_and_validate("MATCH (n) RETURN RIGHT(n.name, 3)").is_ok());
}

// ── DurationBetween expression (line 1311) ──

#[test]
fn valid_duration_between_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN DURATION_BETWEEN(n.d1, n.d2)").is_ok());
}

// ── ALL_DIFFERENT / SAME expressions ──

#[test]
fn valid_all_different_expr() {
    assert!(
        parse_and_validate("MATCH (n) LET x = [1, 2, 3] RETURN ALL_DIFFERENT(x)").is_ok()
            || parse_and_validate("MATCH (n) RETURN ALL_DIFFERENT([n.x, n.y])").is_ok()
    );
}

// ── FILTER statement in linear query (line 366-368) ──

#[test]
fn valid_filter_statement() {
    assert!(parse_and_validate("MATCH (n) FILTER n.age > 30 RETURN n").is_ok());
}

// ── Session statement at statement level (line 131) ──

#[test]
fn valid_session_reset_as_statement() {
    assert!(parse_and_validate("SESSION RESET").is_ok());
}

// ── CREATE GRAPH with type reference (not inline) ──

#[test]
fn valid_create_graph_with_type_ref() {
    assert!(
        parse_and_validate("CREATE GRAPH myGraph :: myType").is_ok()
            || parse_and_validate("CREATE GRAPH myGraph TYPED myType").is_ok()
    );
}

// ── Concat expression with unbound var ──

#[test]
fn invalid_concat_unbound_var() {
    let err = parse_and_validate("MATCH (n) RETURN x || n.name")
        .expect_err("expected unbound var in concat");
    assert!(err.to_string().contains("'x'"));
}

// ── Simplified path pattern (line 1182-1184) ──

#[test]
fn valid_simplified_path_pattern() {
    assert!(parse_and_validate("MATCH p = TRAIL (a)-[:KNOWS]->(b) RETURN a, b").is_ok());
}

// ── Path variable in pattern (line 1132-1134) ──

#[test]
fn valid_path_variable_in_match() {
    assert!(parse_and_validate("MATCH p = (a)-[e]->(b) RETURN p").is_ok());
}

// ── RETURN NO BINDINGS (cypher feature, line 541) ──

#[cfg(feature = "cypher")]
#[test]
fn valid_return_no_bindings() {
    // This tests the ReturnBody::NoBindings variant in cypher mode.
    assert!(parse_and_validate("MATCH (n) SET n.x = 1 RETURN NO BINDINGS").is_ok());
}

// ── RETURN empty items (direct AST, line 550-551) ──

#[test]
fn invalid_return_empty_items() {
    let ret = ReturnStatement {
        span: Span::DUMMY,
        set_quantifier: SetQuantifier::None,
        body: ReturnBody::Items {
            items: vec![],
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
        },
    };
    let err =
        validate_return(&ret, &RapidHashSet::default(), &RapidHashSet::default()).unwrap_err();
    assert!(
        err.to_string()
            .contains("RETURN must have at least one item")
    );
}

// ── SELECT empty items (direct AST, line 653-654) ──

#[test]
fn invalid_select_empty_items() {
    let sel = SelectStatement {
        span: Span::DUMMY,
        set_quantifier: SetQuantifier::None,
        source: None,
        body: SelectBody::Items {
            items: vec![],
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
        },
    };
    let err =
        validate_select(&sel, &RapidHashSet::default(), &RapidHashSet::default()).unwrap_err();
    assert!(
        err.to_string()
            .contains("SELECT must have at least one item")
    );
}

// ── Graph reference with multi-part name (line 905-906) ──

#[test]
fn valid_graph_reference_multipart() {
    // Multi-part names skip the single-ident check.
    let name = ObjectName {
        parts: vec!["catalog".into(), "myGraph".into()],
    };
    assert!(
        validate_graph_reference(&name, &RapidHashSet::default(), &RapidHashSet::default()).is_ok()
    );
}

// ── Graph reference with $$ prefix (line 910) ──

#[test]
fn valid_graph_reference_dollar_prefix() {
    let name = ObjectName {
        parts: vec!["$$currentGraph".into()],
    };
    assert!(
        validate_graph_reference(&name, &RapidHashSet::default(), &RapidHashSet::default()).is_ok()
    );
}

// ── CurrentDate/CurrentTime/SessionUser expressions (lines 1283-1290) ──

#[test]
fn valid_current_date_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN CURRENT_DATE").is_ok());
}

#[test]
fn valid_current_time_expr() {
    assert!(
        parse_and_validate("MATCH (n) RETURN CURRENT_TIME").is_ok()
            || parse_and_validate("MATCH (n) RETURN CURRENT_TIMESTAMP").is_ok()
    );
}

#[test]
fn valid_session_user_expr() {
    assert!(
        parse_and_validate("MATCH (n) RETURN SESSION_USER").is_ok()
            || parse_and_validate("MATCH (n) RETURN CURRENT_USER").is_ok()
    );
}

// ── COALESCE with unbound var ──

#[test]
fn invalid_coalesce_unbound_var() {
    let err = parse_and_validate("MATCH (n) RETURN COALESCE(x, 'unknown')")
        .expect_err("expected unbound var in COALESCE");
    assert!(err.to_string().contains("'x'"));
}

// ── Validate IS DIRECTED expression ──

#[test]
fn valid_is_directed_expr() {
    assert!(parse_and_validate("MATCH (a)-[e]-(b) WHERE e IS DIRECTED RETURN a, b").is_ok());
}

// ── FunctionCall expression (line 1519-1524) ──

#[test]
fn valid_custom_function_call() {
    assert!(parse_and_validate("MATCH (n) RETURN myFunc(n.x, n.y)").is_ok());
}

#[test]
fn invalid_function_call_unbound_arg() {
    let err = parse_and_validate("MATCH (n) RETURN myFunc(x)")
        .expect_err("expected unbound var in function call");
    assert!(err.to_string().contains("'x'"));
}

// ── Aggregate with no inner expr (COUNT(*)) ──

#[test]
fn valid_count_star_aggregate() {
    assert!(parse_and_validate("MATCH (n) RETURN COUNT(*)").is_ok());
}

// ── CALL procedure with args (lines 392-394) ──

#[test]
fn valid_call_procedure_with_args() {
    assert!(parse_and_validate("MATCH (n) CALL myproc(n.x, n.y) YIELD z RETURN z").is_ok());
}

#[test]
fn invalid_call_procedure_unbound_arg() {
    let err = parse_and_validate("MATCH (n) CALL myproc(x) YIELD z RETURN z")
        .expect_err("expected unbound arg in CALL");
    assert!(err.to_string().contains("'x'"));
}

// ── CALL procedure with empty name (direct AST, line 1574-1575) ──

#[test]
fn invalid_call_procedure_empty_name() {
    let cp = CallProcedureStatement {
        span: Span::DUMMY,
        optional: false,
        name: ObjectName { parts: vec![] },
        args: vec![],
        yield_items: None,
    };
    let err = validate_call_procedure(&cp).unwrap_err();
    assert!(
        err.to_string()
            .contains("CALL procedure name must not be empty")
    );
}

// ── SELECT with GROUP BY on items and ORDER BY (lines 687-697) ──

#[test]
fn valid_select_items_group_by_and_order_by_grouped() {
    assert!(
        parse_and_validate(
            "SELECT n.name, COUNT(*) FROM myGraph MATCH (n) GROUP BY n.name ORDER BY n.name"
        )
        .is_ok()
    );
}

// ── Parenthesized path pattern (line 1179-1181) ──

#[test]
fn valid_parenthesized_path_pattern() {
    assert!(parse_and_validate("MATCH (a)((x)-[e]->(y))+(b) RETURN a, b").is_ok());
}

// ── SELECT with WHERE in graph match list (line 720-722) ──

#[test]
fn valid_select_from_graph_match_with_where() {
    assert!(parse_and_validate("SELECT n FROM myGraph MATCH (n) WHERE n.age > 30").is_ok());
}

// ── Focused body: no body present (line 410, body is None) ──

#[test]
fn valid_use_graph_alone_before_match() {
    // USE <graph> with separate MATCH following it, tests focused with body=Some(Match).
    assert!(parse_and_validate("USE myGraph MATCH (n) RETURN n").is_ok());
}

// ── ProcedureBindingInitializer::Query (line 517-519) ──

#[test]
fn valid_table_prefix_binding_with_subquery() {
    assert!(parse_and_validate("TABLE t = { MATCH (n) RETURN n } RETURN t").is_ok());
}

// ── SELECT items extends scope for ORDER BY (lines 657-669) ──

#[test]
fn valid_select_items_order_by_alias() {
    assert!(
        parse_and_validate("SELECT n.name AS name FROM myGraph MATCH (n) ORDER BY name").is_ok()
    );
}

// ── Graph type edge with no nodes (skips endpoint validation, line 191) ──

#[test]
fn valid_graph_type_with_only_edges() {
    let def = GraphTypeDefinition {
        span: Span::DUMMY,
        elements: vec![GraphTypeElement::Edge(EdgeTypeDef {
            span: Span::DUMMY,
            keyword: Keyword::new("EDGE"),
            name: Some("Knows".into()),
            label_set: None,
            direction: crate::types::EdgeDirection::PointingRight,
            source: EdgeEndpoint {
                span: Span::DUMMY,
                type_name: None,
                label: None,
            },
            destination: EdgeEndpoint {
                span: Span::DUMMY,
                type_name: None,
                label: None,
            },
            properties: vec![],
        })],
    };
    // No node_refs, so the endpoint validation loop is skipped.
    assert!(validate_graph_type_definition(&def).is_ok());
}

// ── MATCH YIELD alias uniqueness (line 351) ──

#[test]
fn valid_match_yield_with_alias() {
    assert!(parse_and_validate("MATCH (a)-[e]->(b) YIELD a AS x, b AS y RETURN x, y").is_ok());
}

// ── SELECT validates HAVING ──

#[test]
fn valid_select_having_with_aggregate() {
    assert!(
        parse_and_validate(
            "SELECT n.name, COUNT(*) FROM myGraph MATCH (n) GROUP BY n.name HAVING COUNT(*) > 1"
        )
        .is_ok()
    );
}

// ── SELECT with nested graph query ──

#[test]
fn valid_select_from_graph_nested() {
    assert!(parse_and_validate("SELECT n FROM myGraph { MATCH (n) RETURN n }").is_ok());
}

// ── DATE/TIME literal expressions (lines 1049-1061) ──

#[test]
fn valid_date_literal_expr() {
    assert!(parse_and_validate("MATCH (n) RETURN DATE '2024-01-01'").is_ok());
}

// ── AllDifferent / Same expressions ──

#[test]
fn valid_same_expr() {
    assert!(
        parse_and_validate("MATCH (n) RETURN SAME(n.x, n.y)").is_ok()
            || parse_and_validate("MATCH (n) LET x = [1] LET y = [1] RETURN SAME(x, y)").is_ok()
    );
}

// ── Session graph seed (ADR 0011 §2) ──

fn parse_and_validate_with_seed(input: &str, seed: Option<&SessionGraphSeed>) -> VResult {
    let program = parser::parse(input).expect("parse should succeed");
    validate_with_seed(&program, seed)
}

#[test]
fn current_graph_allowed_without_ingress_seed() {
    assert!(parse_and_validate("USE CURRENT_GRAPH MATCH (n) RETURN n").is_ok());
}

#[test]
fn current_graph_rejected_when_ingress_seed_has_no_current() {
    let seed = SessionGraphSeed::default();
    let err = parse_and_validate_with_seed("USE CURRENT_GRAPH MATCH (n) RETURN n", Some(&seed))
        .expect_err("expected CURRENT_GRAPH rejection");
    assert!(
        err.to_string().contains("CURRENT_GRAPH"),
        "unexpected error: {err}"
    );
}

#[test]
fn current_graph_ok_with_ingress_seed() {
    let seed = SessionGraphSeed {
        current_graph: Some("tenant_a".into()),
        home_graph: Some("tenant_a".into()),
    };
    assert!(
        parse_and_validate_with_seed("USE CURRENT_GRAPH MATCH (n) RETURN n", Some(&seed)).is_ok()
    );
}

#[test]
fn home_graph_rejected_when_ingress_seed_has_no_home() {
    let seed = SessionGraphSeed {
        current_graph: Some("tenant_a".into()),
        home_graph: None,
    };
    let err = parse_and_validate_with_seed("USE HOME_GRAPH MATCH (n) RETURN n", Some(&seed))
        .expect_err("expected HOME_GRAPH rejection");
    assert!(
        err.to_string().contains("HOME_GRAPH"),
        "unexpected error: {err}"
    );
}

#[test]
fn ingress_seed_seeds_graph_scope_for_bound_graph_variable() {
    let seed = SessionGraphSeed {
        current_graph: Some("g".into()),
        home_graph: Some("g".into()),
    };
    assert!(parse_and_validate_with_seed("USE g MATCH (n) RETURN n", Some(&seed)).is_ok());
}

// ── Graph type schema bridge (ADR 0013 §4) ──

use crate::ast::ValueType;
use crate::type_check::schema::{NoSchema, PropertySchema};

struct DirectedKnowsSchema;

impl PropertySchema for DirectedKnowsSchema {
    fn node_property_types(&self, _labels: &[String]) -> Vec<(String, ValueType, bool)> {
        vec![]
    }

    fn edge_property_types(&self, _label: &str) -> Vec<(String, ValueType, bool)> {
        vec![]
    }

    fn edge_is_undirected(&self, label: &str) -> Option<bool> {
        (label == "KNOWS").then_some(false)
    }
}

fn statement_block_from(input: &str) -> StatementBlock {
    parser::parse(input)
        .expect("parse")
        .transaction_activity
        .expect("tx")
        .body
        .expect("body")
}

#[test]
fn validate_block_schema_rejects_undirected_match_on_directed_edge() {
    let block = statement_block_from("MATCH ()~[:KNOWS]~() RETURN 1");
    let err = validate_block_schema_with_seed(&block, None, &DirectedKnowsSchema).unwrap_err();
    assert!(
        err.to_string().contains("DIRECTED"),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_block_schema_is_noop_with_open_world_schema() {
    let block = statement_block_from("MATCH ()~[:KNOWS]~() RETURN 1");
    validate_block_schema_with_seed(&block, None, &NoSchema).expect("open schema");
}
