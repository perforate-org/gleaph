//! Tests for invalid / malformed GQL inputs.
//!
//! Organized by the validation layer that should reject the input:
//!   1. Lexer errors   — malformed tokens, unterminated literals
//!   2. Parser errors  — syntax violations, incomplete statements
//!   3. Validation errors — semantic constraint violations
//!   4. Type-check warnings — provably wrong type combinations

use gleaph_gql::parser;
use gleaph_gql::validate::validate;

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Expects parse to fail (lexer or parser error).
fn parse_err(input: &str) {
    assert!(
        parser::parse(input).is_err(),
        "expected parse error for: {input}"
    );
}

/// Expects parse to succeed but validation to fail.
fn parse_validate_err(input: &str) {
    let program =
        parser::parse(input).unwrap_or_else(|e| panic!("parse failed: {e}\ninput: {input}"));
    assert!(
        validate(&program).is_err(),
        "expected validation error for: {input}"
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// 1. Lexer errors
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn lexer_unterminated_single_quoted_string() {
    parse_err("MATCH (n) WHERE n.name = 'unterminated RETURN n");
}

#[test]
fn lexer_unterminated_double_quoted_ident() {
    parse_err("MATCH (\"unterminated) RETURN n");
}

#[test]
fn lexer_unterminated_backtick_ident() {
    parse_err("MATCH (`unterminated) RETURN n");
}

#[test]
fn lexer_unterminated_no_escape_literal() {
    parse_err("RETURN @'no end");
}

#[test]
fn lexer_unterminated_byte_literal() {
    parse_err("RETURN X'AB");
}

#[test]
fn lexer_unterminated_byte_literal_odd_digits() {
    parse_err("RETURN X'A'");
}

#[test]
fn lexer_bare_dollar_param() {
    parse_err("MATCH (n) WHERE n.age > $ RETURN n");
}

#[test]
fn lexer_bare_double_dollar_param() {
    parse_err("MATCH (n) WHERE $$ > 1 RETURN n");
}

#[test]
fn lexer_unknown_escape_sequence() {
    parse_err("RETURN 'hello\\z'");
}

#[test]
fn lexer_unicode_escape_too_short() {
    parse_err("RETURN '\\u00'");
}

#[test]
fn lexer_unicode_escape_surrogate() {
    parse_err("RETURN '\\uD800'");
}

#[test]
fn lexer_invalid_hex_in_byte_literal() {
    parse_err("RETURN X'GG'");
}

#[test]
fn lexer_empty_hex_prefix() {
    parse_err("RETURN 0x");
}

#[test]
fn lexer_empty_octal_prefix() {
    parse_err("RETURN 0o");
}

#[test]
fn lexer_empty_binary_prefix() {
    parse_err("RETURN 0b");
}

#[test]
fn lexer_unterminated_escape_at_eof() {
    parse_err("RETURN 'test\\");
}

// ════════════════════════════════════════════════════════════════════════════════
// 2. Parser errors
// ════════════════════════════════════════════════════════════════════════════════

/// Empty input, whitespace-only, and comment-only are valid (empty program).
/// These tests verify that the parser accepts them without error.
#[test]
fn parser_empty_input_is_valid() {
    // Empty GQL program is allowed by the spec.
    let _ = parser::parse("").expect("empty input should parse as empty program");
}

#[test]
fn parser_whitespace_only_is_valid() {
    let _ = parser::parse("   ").expect("whitespace-only should parse as empty program");
}

#[test]
fn parser_comment_only_is_valid() {
    let _ = parser::parse("// just a comment").expect("comment-only should parse as empty program");
}

#[test]
fn parser_single_keyword_match() {
    parse_err("MATCH");
}

#[test]
fn parser_single_keyword_return() {
    parse_err("RETURN");
}

#[test]
fn parser_return_trailing_comma() {
    parse_err("MATCH (n) RETURN n,");
}

#[test]
fn parser_unrecognized_statement() {
    parse_err("FOOBAR (n) RETURN n");
}

#[test]
fn parser_garbage_after_statement() {
    parse_err("MATCH (n) RETURN n FOOBAR");
}

#[test]
fn parser_incomplete_case_no_end() {
    parse_err("RETURN CASE WHEN true THEN 1");
}

#[test]
fn parser_incomplete_case_when_no_then() {
    parse_err("RETURN CASE WHEN END");
}

#[test]
fn parser_missing_where_condition() {
    parse_err("MATCH (n) WHERE RETURN n");
}

#[test]
fn parser_set_no_items() {
    parse_err("MATCH (n) SET RETURN n");
}

#[test]
fn parser_remove_no_items() {
    parse_err("MATCH (n) REMOVE RETURN n");
}

#[test]
fn parser_insert_no_pattern() {
    parse_err("INSERT");
}

#[test]
fn parser_return_only_operator() {
    parse_err("RETURN +");
}

#[test]
fn parser_unclosed_parenthesis() {
    parse_err("MATCH (n RETURN n");
}

#[test]
fn parser_unclosed_bracket() {
    parse_err("MATCH (a)-[e->(b) RETURN a");
}

#[test]
fn parser_double_comma_in_return() {
    parse_err("MATCH (n) RETURN n,,n");
}

#[test]
fn parser_order_by_nulls_invalid() {
    parse_err("MATCH (n) RETURN n ORDER BY n.name NULLS FOOBAR");
}

// ════════════════════════════════════════════════════════════════════════════════
// 3. Validation errors
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn validate_unbound_var_in_return() {
    parse_validate_err("MATCH (n) RETURN m");
}

#[test]
fn validate_unbound_var_in_where() {
    parse_validate_err("MATCH (n) WHERE m.age > 1 RETURN n");
}

#[test]
fn validate_unbound_var_in_order_by() {
    parse_validate_err("MATCH (n) RETURN n ORDER BY x");
}

#[test]
fn parser_set_missing_property() {
    // SET without property access (just a variable) is invalid syntax.
    parse_err("MATCH (n) SET n RETURN n");
}

#[test]
fn parser_remove_missing_property() {
    // REMOVE without property specification is invalid syntax.
    parse_err("MATCH (n) REMOVE n RETURN n");
}

// ── SET/REMOVE scope validation ──────────────────────────────────────────────

#[test]
fn validate_set_unbound_var_property() {
    parse_validate_err("MATCH (n) SET m.name = 'x' RETURN n");
}

#[test]
fn validate_set_unbound_var_all_properties() {
    parse_validate_err("MATCH (n) SET m = { name: 'x' } RETURN n");
}

#[test]
fn validate_set_unbound_var_label() {
    parse_validate_err("MATCH (n) SET m :Admin RETURN n");
}

#[test]
fn validate_set_unbound_value_expr() {
    // Target is bound but value references unbound variable.
    parse_validate_err("MATCH (n) SET n.name = m.name RETURN n");
}

#[test]
fn validate_remove_unbound_var_property() {
    parse_validate_err("MATCH (n) REMOVE m.name RETURN n");
}

#[test]
fn validate_remove_unbound_var_label() {
    parse_validate_err("MATCH (n) REMOVE m :Admin RETURN n");
}

#[test]
fn validate_set_bound_var_ok() {
    // SET with bound variable should succeed.
    let input = "MATCH (n) SET n.name = 'x' RETURN n";
    let program = parser::parse(input).unwrap();
    validate(&program).unwrap();
}

#[test]
fn validate_remove_bound_var_ok() {
    // REMOVE with bound variable should succeed.
    let input = "MATCH (n) REMOVE n.name RETURN n";
    let program = parser::parse(input).unwrap();
    validate(&program).unwrap();
}

#[test]
fn validate_unbound_var_in_delete() {
    parse_validate_err("MATCH (n) DELETE m RETURN n");
}

#[test]
fn validate_path_quantifier_inverted_range() {
    parse_validate_err("MATCH (a)-[e]->{5,2}(b) RETURN a");
}

#[test]
fn validate_composite_mismatched_column_names() {
    parse_validate_err("MATCH (n) RETURN n AS x UNION MATCH (m) RETURN m AS y");
}

#[test]
fn validate_composite_different_column_count() {
    parse_validate_err("MATCH (n) RETURN n AS x, n AS y UNION MATCH (m) RETURN m AS x");
}

#[test]
fn validate_group_by_ungrouped_non_aggregate() {
    parse_validate_err("MATCH (n) RETURN n.name, n.age GROUP BY n.name");
}

#[test]
fn validate_contradictory_transaction_modes() {
    parse_validate_err("START TRANSACTION READ ONLY, READ WRITE RETURN 1");
}

#[test]
fn validate_yield_duplicate_alias() {
    parse_validate_err("MATCH (a)-[e]->(b) YIELD a AS x, b AS x RETURN x");
}

#[test]
fn validate_call_duplicate_yield() {
    parse_validate_err("CALL myProc() YIELD x AS a, y AS a RETURN a");
}

#[test]
fn validate_call_inline_duplicate_param() {
    parse_validate_err("MATCH (n) CALL (n, n) { RETURN n } RETURN n");
}

#[test]
fn validate_call_inline_unbound_param() {
    parse_validate_err("MATCH (n) CALL (m) { RETURN m } RETURN n");
}

#[test]
fn validate_nested_exists_unbound_var() {
    parse_validate_err("MATCH (n) WHERE EXISTS { MATCH (a) RETURN m } RETURN n");
}

#[test]
fn validate_use_with_value_binding() {
    parse_validate_err("VALUE g = 1 USE g MATCH (n) RETURN n");
}

#[test]
fn validate_select_group_by_ungrouped() {
    parse_validate_err("SELECT n.name, n.age FROM myGraph MATCH (n) GROUP BY n.name");
}

#[test]
fn validate_select_order_by_unbound() {
    parse_validate_err("SELECT n.name AS name FROM myGraph MATCH (n) ORDER BY other");
}

#[test]
fn validate_select_having_unbound() {
    parse_validate_err(
        "SELECT n.name, COUNT(*) FROM myGraph MATCH (n) GROUP BY n.name HAVING n.age > 1",
    );
}

#[test]
fn validate_call_external_unbound_result() {
    parse_validate_err("MATCH (n) CALL myproc() RETURN x");
}

#[test]
fn validate_call_inline_body_references_outer_unbound() {
    parse_validate_err("MATCH (n)-[:KNOWS]->(m) CALL (n) { RETURN m } RETURN n");
}

#[test]
fn validate_composite_call_mismatched_bindings() {
    parse_validate_err("MATCH (n) CALL { RETURN n AS x UNION RETURN n AS y } RETURN x");
}

// ════════════════════════════════════════════════════════════════════════════════
// 3b. DDL parse errors (§12)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn parser_create_invalid_object() {
    parse_err("CREATE TABLE foo");
}

#[test]
fn parser_drop_invalid_object() {
    parse_err("DROP TABLE foo");
}

#[test]
fn parser_create_graph_type_missing_direction() {
    // EDGE without DIRECTED/UNDIRECTED keyword.
    parse_err("CREATE GRAPH TYPE myType { NODE A LABEL A, EDGE E CONNECTING (A -> A) }");
}

// ── DDL validation errors ────────────────────────────────────────────────────

#[test]
fn validate_graph_type_duplicate_node_name() {
    parse_validate_err(
        "CREATE GRAPH TYPE myType { NODE Person LABEL Person {}, NODE Person LABEL Person2 {} }",
    );
}

#[test]
fn validate_graph_type_duplicate_edge_name() {
    parse_validate_err(
        "CREATE GRAPH TYPE myType { NODE A LABEL A, NODE B LABEL B, \
         DIRECTED EDGE R LABEL R CONNECTING (A -> B), \
         DIRECTED EDGE R LABEL R2 CONNECTING (A -> B) }",
    );
}

#[test]
fn validate_graph_type_duplicate_property() {
    parse_validate_err("CREATE GRAPH TYPE myType { NODE A LABEL A { name STRING, name INT32 } }");
}

#[test]
fn validate_graph_type_edge_endpoint_not_found() {
    parse_validate_err(
        "CREATE GRAPH TYPE myType { NODE A LABEL A, DIRECTED EDGE E LABEL E CONNECTING (A -> B) }",
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// 3c. Session parse errors (§7)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn parser_session_invalid_keyword() {
    parse_err("SESSION FOOBAR");
}

#[test]
fn parser_session_set_invalid_target() {
    parse_err("SESSION SET FOOBAR");
}

#[test]
fn parser_session_set_table_missing_param() {
    // TABLE requires $param, not a bare identifier.
    parse_err("SESSION SET TABLE x = null");
}

#[test]
fn parser_session_set_value_missing_param() {
    // VALUE requires $param, not a bare identifier.
    parse_err("SESSION SET VALUE x = 42");
}

#[test]
fn parser_session_reset_all_invalid() {
    parse_err("SESSION RESET ALL FOOBAR");
}

#[test]
fn parser_session_reset_invalid_target() {
    parse_err("SESSION RESET FOOBAR");
}

#[test]
fn parser_start_transaction_read_invalid() {
    parse_err("START TRANSACTION READ FOOBAR RETURN 1");
}

// ════════════════════════════════════════════════════════════════════════════════
// 3d. Expression parse errors
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn parser_exists_missing_body() {
    parse_err("MATCH (n) WHERE EXISTS n RETURN n");
}

#[test]
fn parser_double_dollar_as_value_expr() {
    // $$param is only valid for graph/schema references, not value expressions.
    parse_err("MATCH (n) WHERE n.x = $$param RETURN n");
}

#[test]
fn parser_limit_non_numeric() {
    parse_err("MATCH (n) RETURN n LIMIT foo");
}

#[test]
fn parser_offset_non_numeric() {
    parse_err("MATCH (n) RETURN n OFFSET foo");
}

// ════════════════════════════════════════════════════════════════════════════════
// 3e. Pattern parse errors
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn parser_match_different_invalid_keyword() {
    // DIFFERENT requires EDGES/RELATIONSHIPS/ELEMENT/ELEMENTS.
    parse_err("MATCH DIFFERENT FOOBAR (n) RETURN n");
}

// ════════════════════════════════════════════════════════════════════════════════
// 3f. Type annotation parse errors (§18.9)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn parser_signed_without_integer_type() {
    parse_err("SESSION SET VALUE $x :: SIGNED FLOAT = 1");
}

#[test]
fn parser_zoned_invalid_type() {
    parse_err("SESSION SET VALUE $x :: ZONED INT32 = 1");
}

#[test]
fn parser_local_invalid_type() {
    parse_err("SESSION SET VALUE $x :: LOCAL INT32 = 1");
}

#[test]
fn parser_interval_invalid_qualifier() {
    parse_err("SESSION SET VALUE $x :: INTERVAL FOOBAR = 1");
}

// ════════════════════════════════════════════════════════════════════════════════
// 4. Type-check warnings (using strict mode where available, or warning checks)
// ════════════════════════════════════════════════════════════════════════════════

mod type_check_invalid {
    use gleaph_gql::ast::{Keyword, ValueType};
    use gleaph_gql::parser;
    use gleaph_gql::type_check::schema::PropertySchema;
    use gleaph_gql::type_check::{NoSchema, WarningKind, type_check_with_schema};
    use gleaph_gql::validate::validate;

    struct TestSchema;

    impl PropertySchema for TestSchema {
        fn node_property_types(&self, labels: &[String]) -> Vec<(String, ValueType, bool)> {
            if labels.contains(&"Person".to_string()) {
                vec![
                    (
                        "name".to_string(),
                        ValueType::String {
                            min_length: None,
                            max_length: None,
                        },
                        true,
                    ),
                    (
                        "age".to_string(),
                        ValueType::Int32 {
                            keyword: Keyword::new("INT32"),
                        },
                        true,
                    ),
                ]
            } else {
                vec![]
            }
        }
        fn edge_property_types(&self, _labels: &str) -> Vec<(String, ValueType, bool)> {
            vec![]
        }
    }

    fn check_warns(input: &str, schema: &dyn PropertySchema, expected: WarningKind) {
        let program = parser::parse(input).unwrap_or_else(|e| panic!("parse error: {e}"));
        validate(&program).unwrap_or_else(|e| panic!("validation error: {e}"));
        let warnings = type_check_with_schema(&program, schema);
        assert!(
            warnings.iter().any(|w| w.kind == expected),
            "expected {:?} warning for: {input}\ngot: {:?}",
            expected,
            warnings,
        );
    }

    fn check_no_warn(input: &str, schema: &dyn PropertySchema, not_expected: WarningKind) {
        let program = parser::parse(input).unwrap_or_else(|e| panic!("parse error: {e}"));
        validate(&program).unwrap_or_else(|e| panic!("validation error: {e}"));
        let warnings = type_check_with_schema(&program, schema);
        assert!(
            !warnings.iter().any(|w| w.kind == not_expected),
            "did not expect {:?} warning for: {input}\ngot: {:?}",
            not_expected,
            warnings,
        );
    }

    #[test]
    fn binary_op_string_plus_int() {
        check_warns(
            "MATCH (n:Person) LET x = n.name + n.age RETURN x",
            &TestSchema,
            WarningKind::BinaryOpMismatch,
        );
    }

    #[test]
    fn comparison_string_gt_int() {
        check_warns(
            "MATCH (n:Person) WHERE n.name > n.age RETURN n",
            &TestSchema,
            WarningKind::ComparisonMismatch,
        );
    }

    #[test]
    fn non_boolean_where_literal() {
        check_warns(
            "MATCH (n:Person) WHERE n.age RETURN n",
            &TestSchema,
            WarningKind::NonBooleanCondition,
        );
    }

    #[test]
    fn valid_boolean_where_no_warning() {
        check_no_warn(
            "MATCH (n:Person) WHERE n.age > 30 RETURN n",
            &TestSchema,
            WarningKind::NonBooleanCondition,
        );
    }

    #[test]
    fn null_check_on_non_null_property() {
        check_warns(
            "MATCH (n:Person) WHERE n.name IS NULL RETURN n",
            &TestSchema,
            WarningKind::NullCheckOnNonNull,
        );
    }

    #[test]
    fn no_warning_without_schema() {
        // Without schema, types are Unknown → no warnings should fire.
        check_no_warn(
            "MATCH (n) WHERE n.age RETURN n",
            &NoSchema,
            WarningKind::NonBooleanCondition,
        );
    }

    // Note: GroupingViolation is caught by the semantic validator (not type checker),
    // so it's tested above in the validation section as validate_group_by_ungrouped_non_aggregate.
}
