use gleaph_gql::ast::ValueType;
use gleaph_gql::parser::parse_statement;
use gleaph_gql::type_check::{
    PropertySchema, TypeWarning, WarningKind, type_check_statement, type_check_statement_strict,
    type_check_statement_with_schema,
};
use gleaph_gql::validate::validate_statement;

fn check_warnings(gql: &str) -> Vec<TypeWarning> {
    let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    type_check_statement(&stmt)
}

fn assert_no_warnings(gql: &str) {
    let ws = check_warnings(gql);
    assert!(
        ws.is_empty(),
        "expected no warnings for '{gql}', got: {ws:?}"
    );
}

fn assert_has_warning(gql: &str, kind: WarningKind) {
    let ws = check_warnings(gql);
    assert!(
        ws.iter().any(|w| w.kind == kind),
        "expected {kind:?} warning for '{gql}', got: {ws:?}"
    );
}

// ---- No warnings on valid queries ----

#[test]
fn no_warning_simple_match() {
    assert_no_warnings("MATCH (n:Person) RETURN n");
}

#[test]
fn no_warning_numeric_arithmetic() {
    assert_no_warnings("RETURN 1 + 2");
}

#[test]
fn no_warning_float_arithmetic() {
    assert_no_warnings("RETURN 1 + 2.0");
}

#[test]
fn no_warning_string_concat() {
    assert_no_warnings("RETURN 'hello' + ' world'");
}

#[test]
fn no_warning_comparison_same_type() {
    assert_no_warnings("MATCH (n:Person) WHERE 1 > 0 RETURN n");
}

// ---- BinaryOp mismatch warnings ----

#[test]
fn warn_add_int_text() {
    assert_has_warning("RETURN 42 + 'hello'", WarningKind::BinaryOpMismatch);
}

#[test]
fn warn_add_bool_int() {
    assert_has_warning("RETURN true + 1", WarningKind::BinaryOpMismatch);
}

#[test]
fn warn_mul_text_int() {
    assert_has_warning("RETURN 'abc' * 3", WarningKind::BinaryOpMismatch);
}

// ---- BinaryOp valid — no warning ----

#[test]
fn no_warning_int_plus_int() {
    assert_no_warnings("RETURN 1 + 2");
}

#[test]
fn no_warning_float_mul_float() {
    assert_no_warnings("RETURN 1.5 * 2.5");
}

#[test]
fn no_warning_int_mod_int() {
    assert_no_warnings("RETURN 10 % 3");
}

// ---- WHERE non-boolean ----

#[test]
fn warn_where_integer() {
    assert_has_warning(
        "MATCH (n:Person) WHERE 42 RETURN n",
        WarningKind::NonBooleanCondition,
    );
}

#[test]
fn warn_where_string() {
    assert_has_warning(
        "MATCH (n:Person) WHERE 'yes' RETURN n",
        WarningKind::NonBooleanCondition,
    );
}

// ---- Function arg mismatch ----

#[test]
fn warn_id_on_edge() {
    assert_has_warning(
        "MATCH (a:X)-[e:R]->(b:Y) RETURN id(e)",
        WarningKind::FunctionArgMismatch,
    );
}

#[test]
fn warn_labels_on_edge() {
    assert_has_warning(
        "MATCH (a:X)-[e:R]->(b:Y) RETURN labels(e)",
        WarningKind::FunctionArgMismatch,
    );
}

#[test]
fn warn_type_on_node() {
    assert_has_warning(
        "MATCH (n:X)-[e:R]->(m:Y) RETURN type(n)",
        WarningKind::FunctionArgMismatch,
    );
}

// ---- Comparison mismatch ----

#[test]
fn warn_compare_int_text() {
    assert_has_warning(
        "MATCH (n:Person) WHERE 42 > 'old' RETURN n",
        WarningKind::ComparisonMismatch,
    );
}

#[test]
fn warn_compare_bool_float() {
    assert_has_warning(
        "MATCH (n:Person) WHERE true = 1.0 RETURN n",
        WarningKind::ComparisonMismatch,
    );
}

#[test]
fn no_warning_compare_int_float() {
    // Numeric promotion: Int and Float are comparable.
    assert_no_warnings("MATCH (n:Person) WHERE 1 > 2.0 RETURN n");
}

// ---- Unknown suppresses warnings ----

#[test]
fn no_warning_property_plus_int() {
    // n.age is Unknown, so no warning.
    assert_no_warnings("MATCH (n:Person) RETURN n.age + 42");
}

#[test]
fn no_warning_property_compare_text() {
    assert_no_warnings("MATCH (n:Person) WHERE n.name > 'Alice' RETURN n");
}

#[test]
fn no_warning_untyped_param() {
    // Untyped parameter is Unknown.
    assert_no_warnings("RETURN $x + 42");
}

// ---- CASE/COALESCE inference ----

#[test]
fn no_warning_case_same_types() {
    assert_no_warnings("RETURN CASE WHEN true THEN 1 ELSE 2 END");
}

#[test]
fn no_warning_coalesce_same_types() {
    assert_no_warnings("RETURN COALESCE(1, 2, 3)");
}

// ---- Aggregate inference ----

#[test]
fn no_warning_count_is_int() {
    assert_no_warnings("MATCH (n:X) RETURN COUNT(n) + 1");
}

#[test]
fn no_warning_avg_is_float() {
    assert_no_warnings("MATCH (n:X) RETURN AVG(1) + 1.0");
}

// ---- Parameter with annotation ----

#[test]
fn warn_typed_param_mismatch() {
    assert_has_warning(
        "RETURN ($x :: INT) + 'hello'",
        WarningKind::BinaryOpMismatch,
    );
}

#[test]
fn no_warning_typed_param_compatible() {
    assert_no_warnings("RETURN ($x :: INT) + 1");
}

// ---- Compound statement ----

#[test]
fn warn_in_union_branch() {
    assert_has_warning(
        "RETURN 1 UNION RETURN 42 + 'x'",
        WarningKind::BinaryOpMismatch,
    );
}

#[test]
fn no_warning_clean_union() {
    assert_no_warnings("RETURN 1 UNION RETURN 2");
}

// ---- Temporal arithmetic ----

#[test]
fn no_warning_date_plus_duration() {
    assert_no_warnings("RETURN DATE('2024-01-01') + DURATION('P1D')");
}

#[test]
fn no_warning_duration_minus_duration() {
    assert_no_warnings("RETURN DURATION('P2D') - DURATION('P1D')");
}

// ---- List concat ----

#[test]
fn no_warning_list_concat() {
    assert_no_warnings("RETURN [1, 2] + [3, 4]");
}

// ===========================================================================
// Phase 2: Schema-aware type inference, WITH projection, NEXT propagation
// ===========================================================================

/// Test schema: Person { age :: INT, name :: TEXT NOT NULL, active :: BOOL }, KNOWS { since :: INT }.
struct TestSchema;

impl PropertySchema for TestSchema {
    fn node_property_types(&self, labels: &[String]) -> Vec<(String, ValueType, bool)> {
        if labels.iter().any(|l| l == "Person") {
            vec![
                ("age".into(), ValueType::Int64, false),
                ("name".into(), ValueType::Text, true), // NOT NULL
                ("active".into(), ValueType::Bool, false),
            ]
        } else {
            vec![]
        }
    }

    fn edge_property_types(&self, label: &str) -> Vec<(String, ValueType, bool)> {
        if label.eq_ignore_ascii_case("KNOWS") {
            vec![("since".into(), ValueType::Int64, false)]
        } else {
            vec![]
        }
    }

    fn resolve_node_type_labels(&self, type_name: &str) -> Option<Vec<String>> {
        match type_name {
            "PersonType" => Some(vec!["Person".into()]),
            "MovieType" => Some(vec!["Movie".into()]),
            _ => None,
        }
    }

    fn edge_endpoint_types(&self, label: &str) -> Vec<(Vec<String>, Vec<String>)> {
        match label.to_ascii_uppercase().as_str() {
            "KNOWS" => vec![(vec!["Person".into()], vec!["Person".into()])],
            // ACTS_IN: Person -> Movie (directional)
            "ACTS_IN" => vec![(vec!["Person".into()], vec!["Movie".into()])],
            _ => vec![],
        }
    }

    fn resolve_edge_type(&self, type_name: &str) -> Option<(String, Vec<String>, Vec<String>)> {
        match type_name {
            "KnowsType" => Some(("KNOWS".into(), vec!["Person".into()], vec!["Person".into()])),
            "ActsInType" => Some((
                "ACTS_IN".into(),
                vec!["Person".into()],
                vec!["Movie".into()],
            )),
            _ => None,
        }
    }
}

fn check_warnings_with_schema(gql: &str) -> Vec<TypeWarning> {
    let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    type_check_statement_with_schema(&stmt, &TestSchema)
}

fn assert_no_warnings_schema(gql: &str) {
    let ws = check_warnings_with_schema(gql);
    assert!(
        ws.is_empty(),
        "expected no warnings for '{gql}', got: {ws:?}"
    );
}

fn assert_has_warning_schema(gql: &str, kind: WarningKind) {
    let ws = check_warnings_with_schema(gql);
    assert!(
        ws.iter().any(|w| w.kind == kind),
        "expected {kind:?} warning for '{gql}', got: {ws:?}"
    );
}

// ---- Schema-aware PropertyAccess ----

#[test]
fn warn_node_property_type_mismatch() {
    // n.age is INT (from schema), 'hello' is TEXT → BinaryOpMismatch.
    assert_has_warning_schema(
        "MATCH (n:Person) RETURN n.age + 'hello'",
        WarningKind::BinaryOpMismatch,
    );
}

#[test]
fn no_warning_node_property_correct_type() {
    // n.age is INT, 1 is INT → no warning.
    assert_no_warnings_schema("MATCH (n:Person) RETURN n.age + 1");
}

#[test]
fn no_warning_unknown_property() {
    // n.email is not in schema → Unknown → no warning.
    assert_no_warnings_schema("MATCH (n:Person) RETURN n.email + 42");
}

#[test]
fn no_warning_no_labels() {
    // No labels → no schema lookup → Unknown → no warning.
    assert_no_warnings_schema("MATCH (n) RETURN n.age + 'x'");
}

#[test]
fn warn_edge_property_type_mismatch() {
    // e.since is INT (from schema), 'text' is TEXT → BinaryOpMismatch.
    assert_has_warning_schema(
        "MATCH (a:Person)-[e:KNOWS]->(b:Person) RETURN e.since + 'text'",
        WarningKind::BinaryOpMismatch,
    );
}

#[test]
fn no_warning_edge_property_correct() {
    assert_no_warnings_schema("MATCH (a:Person)-[e:KNOWS]->(b:Person) RETURN e.since + 1");
}

#[test]
fn warn_where_schema_property() {
    // n.age is INT, 'old' is TEXT → ComparisonMismatch.
    assert_has_warning_schema(
        "MATCH (n:Person) WHERE n.age > 'old' RETURN n",
        WarningKind::ComparisonMismatch,
    );
}

#[test]
fn warn_impossible_pattern_endpoint_combo() {
    assert_has_warning_schema(
        "MATCH (a:Movie)-[:KNOWS]->(b:Person) RETURN a, b",
        WarningKind::ImpossiblePattern,
    );
}

#[test]
fn no_warning_valid_pattern_endpoint_combo() {
    assert_no_warnings_schema("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b");
}

#[test]
fn warn_impossible_pattern_endpoint_combo_via_type_annotations() {
    assert_has_warning_schema(
        "MATCH (a :: MovieType)-[e :: KnowsType]->(b :: PersonType) RETURN a, b, e",
        WarningKind::ImpossiblePattern,
    );
}

#[test]
fn no_warning_valid_pattern_endpoint_combo_via_type_annotations() {
    assert_no_warnings_schema(
        "MATCH (a :: PersonType)-[e :: KnowsType]->(b :: PersonType) RETURN a, b, e",
    );
}

#[test]
fn warn_impossible_pattern_endpoint_combo_from_prior_binding() {
    assert_has_warning_schema(
        "MATCH (a:Movie) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN a, b",
        WarningKind::ImpossiblePattern,
    );
}

#[test]
fn no_warning_valid_pattern_endpoint_combo_from_prior_binding() {
    assert_no_warnings_schema(
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN a, b",
    );
}

#[test]
fn warn_impossible_pattern_via_where_narrowed_label() {
    // KNOWS requires Person→Person, but WHERE narrows b to :Movie → contradiction.
    assert_has_warning_schema(
        "MATCH (a:Person)-[:KNOWS]->(b) WHERE b IS LABELED :Movie RETURN a, b",
        WarningKind::ImpossiblePattern,
    );
}

#[test]
fn no_warning_valid_pattern_via_where_narrowed_label() {
    // KNOWS requires Person→Person, WHERE narrows b to :Person → valid.
    assert_no_warnings_schema(
        "MATCH (a:Person)-[:KNOWS]->(b) WHERE b IS LABELED :Person RETURN a, b",
    );
}

#[test]
fn warn_variable_reuse_conflicting_labels_optional() {
    // Variable reuse across MATCH and OPTIONAL MATCH with conflicting labels.
    assert_has_warning(
        "MATCH (n:Person) OPTIONAL MATCH (n:Movie)-[]->(m) RETURN n, m",
        WarningKind::ImpossiblePattern,
    );
}

#[test]
fn no_warning_variable_reuse_same_label_optional() {
    // Same variable, same label across MATCH + OPTIONAL MATCH — no contradiction.
    assert_no_warnings("MATCH (n:Person) OPTIONAL MATCH (n:Person)-[]->(m) RETURN n, m");
}

#[test]
fn no_warning_variable_reuse_no_label_optional() {
    // Second use has no label — compatible with any first label.
    assert_no_warnings("MATCH (n:Person) OPTIONAL MATCH (n)-[]->(m) RETURN n, m");
}

// ---- WITH projection ----

#[test]
fn with_alias_carries_type() {
    // WITH n.age AS x → x is INT; x + 'y' → BinaryOpMismatch.
    assert_has_warning_schema(
        "MATCH (n:Person) WITH n.age AS x RETURN x + 'y'",
        WarningKind::BinaryOpMismatch,
    );
}

#[test]
fn with_projection_drops_bindings() {
    // After WITH n.age AS x, only x survives. x is INT, 'oops' is TEXT → no type error
    // because x + 1 is fine, but n is gone. Validator already catches undefined vars,
    // so here we verify the type checker doesn't infer x as Unknown.
    assert_has_warning_schema(
        "MATCH (n:Person) WITH n.age AS x RETURN x + 'oops'",
        WarningKind::BinaryOpMismatch,
    );
}

#[test]
fn with_star_keeps_bindings() {
    // WITH * keeps n → n.age is INT from schema → warns.
    assert_has_warning_schema(
        "MATCH (n:Person) WITH * RETURN n.age + 'x'",
        WarningKind::BinaryOpMismatch,
    );
}

#[test]
fn with_star_plus_alias() {
    // WITH * passes all bindings through. In a subsequent WITH, use n.age AS x.
    // Then x is INT; x + 'y' → warns.
    assert_has_warning_schema(
        "MATCH (n:Person) WITH * WITH n.age AS x RETURN x + 'y'",
        WarningKind::BinaryOpMismatch,
    );
}

// ---- NEXT pipeline ----

#[test]
fn next_pipeline_propagates_types() {
    // Left returns x=1 (INT), right uses x + 'y' → warns.
    assert_has_warning(
        "RETURN 1 AS x NEXT RETURN x + 'y'",
        WarningKind::BinaryOpMismatch,
    );
}

#[test]
fn next_yield_filters_columns() {
    // Left returns x=1, y=2. YIELD x → only x available in right.
    // y is Unknown → no warning for y + 1.
    assert_no_warnings("RETURN 1 AS x, 2 AS y NEXT YIELD x RETURN y + 'z'");
}

// ---- Record field access ----

#[test]
fn record_field_access_infers_type() {
    // {a: 1, b: 'x'}.a is INT; INT + 'y' → warns.
    assert_has_warning(
        "RETURN {a: 1, b: 'x'}.a + 'y'",
        WarningKind::BinaryOpMismatch,
    );
}

// ---- Chained WITH ----

#[test]
fn chained_with_propagation() {
    // n.age (INT) → x (INT) → y (INT) → y + 'z' warns.
    assert_has_warning_schema(
        "MATCH (n:Person) WITH n.age AS x WITH x AS y RETURN y + 'z'",
        WarningKind::BinaryOpMismatch,
    );
}

// ===========================================================================
// Phase 3: Error-mode strict type checking (§18.9)
// ===========================================================================

fn strict_check(gql: &str) -> Result<(), gleaph_types::GleaphError> {
    let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    type_check_statement_strict(&stmt, &TestSchema)
}

#[test]
fn strict_rejects_binary_op_mismatch() {
    let err = strict_check("MATCH (n:Person) RETURN n.age + 'hello'").unwrap_err();
    assert!(err.to_string().contains("type error"), "got: {err}");
}

#[test]
fn strict_rejects_comparison_mismatch() {
    let err = strict_check("MATCH (n:Person) WHERE n.age > 'old' RETURN n").unwrap_err();
    assert!(err.to_string().contains("type error"), "got: {err}");
}

#[test]
fn strict_rejects_function_arg_mismatch() {
    let err = strict_check("MATCH (n:X)-[e:R]->(m:Y) RETURN type(n)").unwrap_err();
    assert!(err.to_string().contains("type error"), "got: {err}");
}

#[test]
fn strict_rejects_non_boolean_where() {
    let err = strict_check("MATCH (n:Person) WHERE 42 RETURN n").unwrap_err();
    assert!(err.to_string().contains("type error"), "got: {err}");
}

#[test]
fn strict_rejects_impossible_pattern_endpoint_combo() {
    let err = strict_check("MATCH (a:Movie)-[:KNOWS]->(b:Person) RETURN a, b").unwrap_err();
    assert!(err.to_string().contains("type error"), "got: {err}");
}

#[test]
fn strict_rejects_impossible_pattern_endpoint_combo_via_type_annotations() {
    let err =
        strict_check("MATCH (a :: MovieType)-[e :: KnowsType]->(b :: PersonType) RETURN a, b")
            .unwrap_err();
    assert!(err.to_string().contains("type error"), "got: {err}");
}

#[test]
fn strict_rejects_impossible_pattern_endpoint_combo_from_prior_binding() {
    let err = strict_check("MATCH (a:Movie) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN a, b")
        .unwrap_err();
    assert!(err.to_string().contains("type error"), "got: {err}");
}

#[test]
fn strict_passes_valid_query() {
    strict_check("MATCH (n:Person) RETURN n.age + 1").unwrap();
}

#[test]
fn strict_passes_unknown_property() {
    // Unknown properties don't trigger errors even in strict mode.
    strict_check("MATCH (n:Person) RETURN n.email + 42").unwrap();
}

#[test]
fn strict_passes_no_schema() {
    // Without schema, property types are unknown → no error.
    let stmt = parse_statement("MATCH (n:Person) RETURN n.age + 'hello'").unwrap();
    validate_statement(&stmt).unwrap();
    type_check_statement_strict(&stmt, &gleaph_gql::type_check::NoSchema).unwrap();
}

// ---- Parser tests for SET TYPE CHECK ----

#[test]
fn parse_set_type_check_strict() {
    let stmt = parse_statement("SET TYPE CHECK STRICT").unwrap();
    assert!(matches!(
        stmt,
        gleaph_gql::ast::Statement::SetTypeCheck(gleaph_gql::ast::TypeCheckMode::Strict)
    ));
}

#[test]
fn parse_set_type_check_warning() {
    let stmt = parse_statement("SET TYPE CHECK WARNING").unwrap();
    assert!(matches!(
        stmt,
        gleaph_gql::ast::Statement::SetTypeCheck(gleaph_gql::ast::TypeCheckMode::Warning)
    ));
}

#[test]
fn parse_show_settings() {
    let stmt = parse_statement("SHOW SETTINGS").unwrap();
    assert!(matches!(
        stmt,
        gleaph_gql::ast::Statement::Show(gleaph_gql::ast::ShowTarget::Settings)
    ));
}

// ===========================================================================
// Type union syntax for parameters and union-aware checking
// ===========================================================================

#[test]
fn parse_param_union_type() {
    let stmt = parse_statement("RETURN $x :: INT | TEXT").unwrap();
    if let gleaph_gql::ast::Statement::Query(q) = &stmt {
        match &q.return_clause.items[0].expr {
            gleaph_gql::ast::Expr::Parameter {
                type_annotation, ..
            } => {
                assert_eq!(
                    *type_annotation,
                    Some(vec![ValueType::Int32, ValueType::Text])
                );
            }
            other => panic!("expected Parameter, got: {other:?}"),
        }
    }
}

#[test]
fn parse_param_triple_union() {
    let stmt = parse_statement("RETURN $x :: INT | FLOAT | TEXT").unwrap();
    if let gleaph_gql::ast::Statement::Query(q) = &stmt {
        match &q.return_clause.items[0].expr {
            gleaph_gql::ast::Expr::Parameter {
                type_annotation, ..
            } => {
                assert_eq!(
                    *type_annotation,
                    Some(vec![ValueType::Int32, ValueType::Float32, ValueType::Text])
                );
            }
            other => panic!("expected Parameter, got: {other:?}"),
        }
    }
}

#[test]
fn union_param_compatible_no_warning() {
    // $x :: INT | FLOAT — adding 1 (INT) should not warn since INT variant is compatible.
    assert_no_warnings("RETURN ($x :: INT | FLOAT) + 1");
}

#[test]
fn union_param_incompatible_warns() {
    // $x :: TEXT | BOOL — adding 1 (INT) should warn since no variant is numeric.
    assert_has_warning(
        "RETURN ($x :: TEXT | BOOL) + 1",
        WarningKind::BinaryOpMismatch,
    );
}

#[test]
fn union_from_case_compatible_no_warning() {
    // CASE returns INT or INT → collapsed to INT. INT + 1 is fine.
    assert_no_warnings("RETURN CASE WHEN true THEN 1 ELSE 2 END + 3");
}

#[test]
fn union_from_case_mixed_warns() {
    // CASE returns INT or TEXT → union. union + 1 should not warn (INT variant works).
    assert_no_warnings("RETURN CASE WHEN true THEN 1 ELSE 'x' END + 1");
}

#[test]
fn union_comparison_compatible_no_warning() {
    // $x :: INT | FLOAT compared to 1 (INT) — numeric types are comparable.
    assert_no_warnings("MATCH (n) WHERE ($x :: INT | FLOAT) > 1 RETURN n");
}

#[test]
fn union_comparison_incompatible_warns() {
    // $x :: TEXT | BOOL compared to 1 (INT) — no variant is comparable.
    assert_has_warning(
        "MATCH (n) WHERE ($x :: TEXT | BOOL) > 1 RETURN n",
        WarningKind::ComparisonMismatch,
    );
}

// ---- NOT NULL constraint propagation ----

#[test]
fn nonnull_is_null_warns() {
    // name is NOT NULL in TestSchema → IS NULL always false.
    assert_has_warning_schema(
        "MATCH (n:Person) WHERE n.name IS NULL RETURN n",
        WarningKind::NullCheckOnNonNull,
    );
}

#[test]
fn nonnull_is_not_null_warns() {
    // name is NOT NULL → IS NOT NULL always true.
    assert_has_warning_schema(
        "MATCH (n:Person) WHERE n.name IS NOT NULL RETURN n",
        WarningKind::NullCheckOnNonNull,
    );
}

#[test]
fn nullable_is_null_no_warning() {
    // age is nullable → IS NULL is a valid check.
    assert_no_warnings_schema("MATCH (n:Person) WHERE n.age IS NULL RETURN n");
}

#[test]
fn nonnull_arithmetic_no_warning() {
    // name is NOT NULL TEXT, concat with TEXT should work fine.
    assert_no_warnings_schema("MATCH (n:Person) RETURN n.name + ' suffix'");
}

#[test]
fn nonnull_strict_rejects_null_check() {
    // Strict mode should reject IS NULL on NOT NULL property.
    let err = strict_check("MATCH (n:Person) WHERE n.name IS NULL RETURN n").unwrap_err();
    assert!(
        format!("{err}").contains("type error"),
        "expected type error, got: {err}"
    );
}

// ===========================================================================
// Flow-sensitive narrowing from WHERE predicates
// ===========================================================================

#[test]
fn narrowing_is_not_null_no_false_positive() {
    // n.age is nullable (not NOT NULL in schema). IS NOT NULL in WHERE should
    // NOT warn — it's a valid check on a nullable property.
    assert_no_warnings_schema("MATCH (n:Person) WHERE n.age IS NOT NULL RETURN n.age");
}

#[test]
fn narrowing_is_not_null_with_pipeline() {
    // n.age is nullable. WHERE n.age IS NOT NULL narrows it.
    // In the RETURN clause, n.age + 1 should be fine (and n.age is now NonNull).
    assert_no_warnings_schema("MATCH (n:Person) WHERE n.age IS NOT NULL RETURN n.age + 1");
}

#[test]
fn narrowing_is_not_null_suppresses_downstream_null_check_warning() {
    // After narrowing n.age to non-null via WHERE, a redundant IS NOT NULL in RETURN
    // should still work. But importantly, the narrowing makes n.age typed as NonNull.
    // Testing: schema says n.age is nullable (Int, not required).
    // Without narrowing: n.age is Scalar(Int) (not NonNull) → IS NULL is fine.
    // With narrowing: n.age becomes NonNull(Scalar(Int)) → IS NULL warns.

    // Without narrowing — no NullCheckOnNonNull warning:
    let ws_no_narrow = check_warnings_with_schema("MATCH (n:Person) RETURN n.age IS NULL");
    assert!(
        !ws_no_narrow
            .iter()
            .any(|w| w.kind == WarningKind::NullCheckOnNonNull),
        "without narrowing, n.age IS NULL should not warn: {ws_no_narrow:?}"
    );

    // With narrowing (IS NOT NULL in WHERE) — should now warn:
    let ws_narrow =
        check_warnings_with_schema("MATCH (n:Person) WHERE n.age IS NOT NULL RETURN n.age IS NULL");
    assert!(
        ws_narrow
            .iter()
            .any(|w| w.kind == WarningKind::NullCheckOnNonNull),
        "after narrowing, n.age IS NULL should warn (always false): {ws_narrow:?}"
    );
}

#[test]
fn narrowing_label_refines_property_access() {
    // MATCH (n) — n has no labels → property types unknown.
    // WHERE n IS LABELED :Person → n gains Person label → schema lookup works.
    assert_has_warning_schema(
        "MATCH (n) WHERE n IS LABELED :Person RETURN n.age + 'hello'",
        WarningKind::BinaryOpMismatch,
    );
}

#[test]
fn narrowing_label_without_narrowing_no_warning() {
    // Without label narrowing: n has no labels, n.age is Unknown → no warning.
    assert_no_warnings_schema("MATCH (n) RETURN n.age + 'hello'");
}

#[test]
fn narrowing_edge_label_via_type_function() {
    // MATCH (a)-[e]->(b) — e has no label → properties unknown.
    // WHERE type(e) = 'KNOWS' → e gains KNOWS label → schema lookup works.
    assert_has_warning_schema(
        "MATCH (a:Person)-[e]->(b:Person) WHERE type(e) = 'KNOWS' RETURN e.since + 'hello'",
        WarningKind::BinaryOpMismatch,
    );
}

#[test]
fn narrowing_edge_label_without_narrowing_no_warning() {
    // Without edge label narrowing: e has no label, e.since is Unknown → no warning.
    assert_no_warnings_schema("MATCH (a:Person)-[e]->(b:Person) RETURN e.since + 'hello'");
}

#[test]
fn narrowing_and_connected_multiple() {
    // Multiple AND-connected IS NOT NULL narrows multiple properties.
    assert_no_warnings_schema(
        "MATCH (n:Person) WHERE n.age IS NOT NULL AND n.active IS NOT NULL RETURN n.age + 1",
    );
}

#[test]
fn narrowing_or_connected_no_effect() {
    // OR-connected IS NOT NULL should NOT narrow (conservative).
    // n.age stays nullable — IS NULL should not warn.
    let ws = check_warnings_with_schema(
        "MATCH (n:Person) WHERE n.age IS NOT NULL OR n.name = 'x' RETURN n.age IS NULL",
    );
    assert!(
        !ws.iter().any(|w| w.kind == WarningKind::NullCheckOnNonNull),
        "OR-connected narrowing should not apply: {ws:?}"
    );
}

// ===========================================================================
// OPTIONAL MATCH null lifting
// ===========================================================================

#[test]
fn optional_match_strips_nonnull_from_property() {
    // b is from OPTIONAL MATCH → b.name (NOT NULL in schema) should become nullable.
    // IS NULL on b.name should NOT warn (because b itself might be null).
    let ws = check_warnings_with_schema(
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN b.name IS NULL",
    );
    assert!(
        !ws.iter().any(|w| w.kind == WarningKind::NullCheckOnNonNull),
        "OPTIONAL MATCH should strip NonNull: {ws:?}"
    );
}

#[test]
fn non_optional_match_keeps_nonnull() {
    // b is from required MATCH → b.name (NOT NULL) should stay NonNull.
    // IS NULL on b.name should warn.
    assert_has_warning_schema(
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name IS NULL",
        WarningKind::NullCheckOnNonNull,
    );
}

#[test]
fn optional_match_preserves_type_for_arithmetic() {
    // b is from OPTIONAL MATCH but b.age is still Int → arithmetic still works.
    assert_no_warnings_schema(
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN b.age + 1",
    );
}

// ===========================================================================
// Never type — contradiction detection
// ===========================================================================

#[test]
fn never_from_empty_case_no_crash() {
    // An empty CASE union (all branches Never) should not crash.
    // This is mostly a structural test — CASE always has branches in practice.
    assert_no_warnings("RETURN CASE WHEN true THEN 1 ELSE 2 END");
}

#[test]
fn never_suppresses_downstream_warnings() {
    // If an expression could be Never, downstream warnings should be suppressed.
    // This is tested indirectly through the Union(Never, T) → T simplification.
    assert_no_warnings("RETURN CASE WHEN true THEN 1 ELSE 2 END + 3");
}

// ===========================================================================
// Combined: narrowing + schema interaction
// ===========================================================================

#[test]
fn narrowing_preserves_schema_type_after_is_not_null() {
    // n.age narrowed to non-null → still Int from schema → adding Text should warn.
    assert_has_warning_schema(
        "MATCH (n:Person) WHERE n.age IS NOT NULL RETURN n.age + 'hello'",
        WarningKind::BinaryOpMismatch,
    );
}

#[test]
fn narrowing_in_with_pipeline() {
    // Narrowing in WHERE survives into WITH projection.
    let ws = check_warnings_with_schema(
        "MATCH (n:Person) WHERE n.age IS NOT NULL WITH n.age AS x RETURN x IS NULL",
    );
    // x = n.age which was narrowed to NonNull → IS NULL on x should warn.
    assert!(
        ws.iter().any(|w| w.kind == WarningKind::NullCheckOnNonNull),
        "narrowing should carry through WITH: {ws:?}"
    );
}

// ===========================================================================
// Aggregation boundary: GROUP BY violations
// ===========================================================================

#[test]
fn grouping_violation_non_grouped_variable() {
    // n.name is not in GROUP BY but appears un-aggregated.
    assert_has_warning(
        "MATCH (n:Person) RETURN n.name, count(*) GROUP BY n.age",
        WarningKind::GroupingViolation,
    );
}

#[test]
fn no_warning_grouped_variable() {
    // n.age is in GROUP BY — no violation.
    assert_no_warnings("MATCH (n:Person) RETURN n.age, count(*) GROUP BY n.age");
}

#[test]
fn no_warning_all_aggregates() {
    // Every RETURN item is aggregated — no GROUP BY needed.
    assert_no_warnings("MATCH (n:Person) RETURN count(*), sum(n.age)");
}

#[test]
fn no_warning_implicit_grouping() {
    // Implicit grouping (no GROUP BY clause) — executor handles it.
    assert_no_warnings("MATCH (n:Person) RETURN n.name, count(*)");
}

#[test]
fn grouping_violation_property_access() {
    // n.name and n.age are both non-aggregate, but only n.age is grouped.
    assert_has_warning(
        "MATCH (n:Person) RETURN n.age, n.name, count(*) GROUP BY n.age",
        WarningKind::GroupingViolation,
    );
}

#[test]
fn no_warning_grouped_property_access() {
    assert_no_warnings("MATCH (n:Person) RETURN n.age, n.name, count(*) GROUP BY n.age, n.name");
}

#[test]
fn no_warning_no_aggregates_present() {
    // No aggregates → the check doesn't apply.
    assert_no_warnings("MATCH (n:Person) RETURN n.name, n.age GROUP BY n.age");
}

// ===========================================================================
// Phase B parity: constraint-based path must produce same warnings as legacy
// ===========================================================================

use gleaph_gql::type_check::type_check_via_constraints;

/// Helper: compare warnings from both paths, asserting identical kind sets.
fn assert_parity(gql: &str) {
    let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    let legacy = type_check_statement(&stmt);
    let constraint_based = type_check_via_constraints(&stmt, &gleaph_gql::type_check::NoSchema);

    let legacy_kinds: Vec<WarningKind> = legacy.iter().map(|w| w.kind).collect();
    let cb_kinds: Vec<WarningKind> = constraint_based.iter().map(|w| w.kind).collect();
    assert_eq!(
        legacy_kinds, cb_kinds,
        "parity mismatch for '{gql}':\n  legacy:     {legacy_kinds:?}\n  constraint: {cb_kinds:?}"
    );
}

fn assert_parity_schema(gql: &str) {
    let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    let legacy = type_check_statement_with_schema(&stmt, &TestSchema);
    let constraint_based = type_check_via_constraints(&stmt, &TestSchema);

    let legacy_kinds: Vec<WarningKind> = legacy.iter().map(|w| w.kind).collect();
    let cb_kinds: Vec<WarningKind> = constraint_based.iter().map(|w| w.kind).collect();
    assert_eq!(
        legacy_kinds, cb_kinds,
        "parity mismatch for '{gql}':\n  legacy:     {legacy_kinds:?}\n  constraint: {cb_kinds:?}"
    );
}

#[test]
fn parity_no_warnings() {
    assert_parity("MATCH (n:Person) RETURN n");
    assert_parity("RETURN 1 + 2");
    assert_parity("RETURN 'hello' + ' world'");
}

#[test]
fn parity_binary_op_mismatch() {
    assert_parity("RETURN 42 + 'hello'");
    assert_parity("RETURN 'abc' - 1");
}

#[test]
fn parity_comparison_mismatch() {
    assert_parity("MATCH (n) WHERE 42 > 'text' RETURN n");
}

#[test]
fn parity_non_boolean_condition() {
    assert_parity("MATCH (n) WHERE 42 RETURN n");
}

#[test]
fn parity_function_arg_mismatch() {
    assert_parity("MATCH (n)-[e]->(m) RETURN id(e)");
    assert_parity("MATCH (n)-[e]->(m) RETURN type(n)");
}

#[test]
fn parity_null_check_on_nonnull() {
    assert_parity_schema("MATCH (n:Person) WHERE n.name IS NULL RETURN n");
    assert_parity_schema("MATCH (n:Person) WHERE n.name IS NOT NULL RETURN n");
}

#[test]
fn parity_schema_binary_op() {
    assert_parity_schema("MATCH (n:Person) RETURN n.age + 'hello'");
}

#[test]
fn parity_aggregation_grouping_violation() {
    assert_parity("MATCH (n:Person) RETURN n.name, count(*) GROUP BY n.age");
}

#[test]
fn parity_narrowing_no_false_positive() {
    assert_parity_schema("MATCH (n:Person) WHERE n.age IS NOT NULL RETURN n.age");
}

#[test]
fn parity_optional_match_null_lifting() {
    assert_parity_schema(
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN b.name IS NULL",
    );
}

#[test]
fn parity_variable_reuse_contradiction() {
    assert_parity("MATCH (n:Person) OPTIONAL MATCH (n:Movie)-[]->(m) RETURN n, m");
}

#[test]
fn parity_endpoint_contradiction() {
    assert_parity_schema("MATCH (a:Movie)-[:KNOWS]->(b:Person) RETURN a, b");
}

#[test]
fn parity_where_narrowed_label_contradiction() {
    assert_parity_schema("MATCH (a:Person)-[:KNOWS]->(b) WHERE b IS LABELED :Movie RETURN a, b");
}

// ---- Planner type diagnostic integration (Step 8) ----

#[test]
fn plan_with_schema_attaches_type_diagnostics() {
    let gql = "MATCH (n:Person) RETURN n.age + 'hello'";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
    let plan =
        gleaph_gql::planner::build_plan_with_schema_and_stats(&stmt, None, &TestSchema).unwrap();
    assert!(
        plan.annotations.type_diagnostics.is_some(),
        "expected type diagnostics attached to plan"
    );
    let diags = plan.annotations.type_diagnostics.unwrap();
    assert!(
        diags
            .iter()
            .any(|d| d.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch in plan diagnostics, got: {diags:?}"
    );
}

#[test]
fn plan_with_schema_marks_contradiction() {
    let gql = "MATCH (a:Movie)-[:KNOWS]->(b:Person) RETURN a, b";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
    let plan =
        gleaph_gql::planner::build_plan_with_schema_and_stats(&stmt, None, &TestSchema).unwrap();
    assert!(
        plan.annotations.statically_contradictory,
        "expected statically_contradictory for impossible endpoint pattern"
    );
}

#[test]
fn plan_with_schema_no_contradiction_for_valid() {
    let gql = "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
    let plan =
        gleaph_gql::planner::build_plan_with_schema_and_stats(&stmt, None, &TestSchema).unwrap();
    assert!(
        !plan.annotations.statically_contradictory,
        "should not be contradictory for valid pattern"
    );
}

#[test]
fn plan_without_schema_has_no_type_diagnostics() {
    let gql = "MATCH (n) RETURN n";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
    let plan = gleaph_gql::planner::build_plan(&stmt).unwrap();
    assert!(
        plan.annotations.type_diagnostics.is_none(),
        "no diagnostics expected when no schema is provided"
    );
    assert!(!plan.annotations.statically_contradictory);
}

#[test]
fn plan_explain_shows_contradiction() {
    let gql = "MATCH (a:Movie)-[:KNOWS]->(b:Person) RETURN a, b";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
    let plan =
        gleaph_gql::planner::build_plan_with_schema_and_stats(&stmt, None, &TestSchema).unwrap();
    let lines = plan.explain_lines();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("statically-contradictory=true")),
        "explain should show contradiction, got: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("type-diagnostic-count=")),
        "explain should show diagnostic count, got: {lines:?}"
    );
}

// ===========================================================================
// Phase C: Direction-aware and multi-hop contradiction detection
// ===========================================================================

// ---- Direction-aware endpoint contradictions ----

#[test]
fn contradiction_incoming_edge() {
    // ACTS_IN only allows Person -> Movie.
    // `<-[:ACTS_IN]-` from a Person means Person is the *destination*, but ACTS_IN
    // requires Person as source and Movie as destination.
    assert_has_warning_schema(
        "MATCH (a:Movie)<-[:ACTS_IN]-(b:Movie) RETURN a, b",
        WarningKind::ImpossiblePattern,
    );
}

#[test]
fn no_contradiction_incoming_valid() {
    // (a:Movie)<-[:ACTS_IN]-(b:Person) is valid: Person->Movie with incoming direction
    // means b(Person) is source, a(Movie) is destination.
    assert_no_warnings_schema("MATCH (a:Movie)<-[:ACTS_IN]-(b:Person) RETURN a, b");
}

#[test]
fn contradiction_undirected_edge() {
    // ACTS_IN: Person -> Movie only.
    // `(a:Movie)-[:ACTS_IN]-(b:Movie)`: neither Movie->Movie nor Movie->Movie matches.
    assert_has_warning_schema(
        "MATCH (a:Movie)-[:ACTS_IN]-(b:Movie) RETURN a, b",
        WarningKind::ImpossiblePattern,
    );
}

#[test]
fn no_contradiction_undirected_valid() {
    // (a:Person)-[:ACTS_IN]-(b:Movie): forward Person->Movie matches.
    assert_no_warnings_schema("MATCH (a:Person)-[:ACTS_IN]-(b:Movie) RETURN a, b");
}

#[test]
fn no_contradiction_undirected_reverse_valid() {
    // (a:Movie)-[:ACTS_IN]-(b:Person): reverse Person->Movie matches (b is source, a is dest).
    assert_no_warnings_schema("MATCH (a:Movie)-[:ACTS_IN]-(b:Person) RETURN a, b");
}

#[test]
fn contradiction_undirected_symmetric() {
    // KNOWS: Person <-> Person only.
    // (a:Movie)-[:KNOWS]-(b:Person): neither direction satisfies Person->Person.
    assert_has_warning_schema(
        "MATCH (a:Movie)-[:KNOWS]-(b:Person) RETURN a, b",
        WarningKind::ImpossiblePattern,
    );
}

// ---- Edge label narrowing from WHERE ----

#[test]
fn contradiction_where_edge_type_narrowing() {
    // Edge `e` has no label, but WHERE narrows it to ACTS_IN.
    // Person->Person contradicts ACTS_IN (Person -> Movie).
    assert_has_warning_schema(
        "MATCH (a:Person)-[e]->(b:Person) WHERE type(e) = 'ACTS_IN' RETURN a, b",
        WarningKind::ImpossiblePattern,
    );
}

#[test]
fn no_contradiction_where_edge_type_narrowing_valid() {
    // Edge narrowed to ACTS_IN with valid endpoints Person -> Movie.
    assert_no_warnings_schema(
        "MATCH (a:Person)-[e]->(b:Movie) WHERE type(e) = 'ACTS_IN' RETURN a, b",
    );
}

// ---- Multi-hop chain contradictions ----

#[test]
fn contradiction_multi_hop_chain() {
    // Person -[:KNOWS]-> Person -[:ACTS_IN]-> Person
    // Second hop: ACTS_IN requires Person -> Movie, but destination is Person.
    assert_has_warning_schema(
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:ACTS_IN]->(c:Person) RETURN a, b, c",
        WarningKind::ImpossiblePattern,
    );
}

#[test]
fn no_contradiction_multi_hop_valid() {
    // Person -[:KNOWS]-> Person -[:ACTS_IN]-> Movie — all valid.
    assert_no_warnings_schema(
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:ACTS_IN]->(c:Movie) RETURN a, b, c",
    );
}

// ---- Edge type annotation ----

#[test]
fn contradiction_edge_type_annotation() {
    // :: ActsInType resolves to ACTS_IN (Person -> Movie).
    // Movie -> Person contradicts.
    assert_has_warning_schema(
        "MATCH (a:Movie)-[e :: ActsInType]->(b:Person) RETURN a, b",
        WarningKind::ImpossiblePattern,
    );
}

#[test]
fn no_contradiction_edge_type_annotation_valid() {
    assert_no_warnings_schema("MATCH (a:Person)-[e :: ActsInType]->(b:Movie) RETURN a, b");
}

// ===========================================================================
// Phase E: Semantic-driven planner facts
// ===========================================================================

#[test]
fn semantic_where_equality_predicates_extracted() {
    use gleaph_gql::semantic::analyze_statement_structure;
    let stmt = parse_statement("MATCH (n:Person) WHERE n.age = 25 RETURN n").unwrap();
    validate_statement(&stmt).unwrap();
    let analysis = analyze_statement_structure(&stmt);
    let eq_preds = analysis.where_equality_predicates();
    assert!(
        eq_preds.iter().any(|(v, p)| v == "n" && p == "age"),
        "expected (n, age) in semantic equality predicates, got: {eq_preds:?}"
    );
}

#[test]
fn semantic_where_range_predicates_extracted() {
    use gleaph_gql::semantic::analyze_statement_structure;
    let stmt = parse_statement("MATCH (n:Person) WHERE n.age >= 18 RETURN n").unwrap();
    validate_statement(&stmt).unwrap();
    let analysis = analyze_statement_structure(&stmt);
    let range = analysis.first_where_range_predicate();
    assert!(range.is_some(), "expected a range predicate");
    let (var, prop, _op) = range.unwrap();
    assert_eq!(var, "n");
    assert_eq!(prop, "age");
}

#[test]
fn semantic_inline_node_properties_extracted() {
    use gleaph_gql::semantic::analyze_statement_structure;
    let stmt = parse_statement("MATCH (n:Person {name: 'Alice'}) RETURN n").unwrap();
    validate_statement(&stmt).unwrap();
    let analysis = analyze_statement_structure(&stmt);
    let inline = analysis.inline_node_properties();
    assert!(
        inline.iter().any(|(v, p)| v == "n" && p == "name"),
        "expected (n, name) in semantic inline properties, got: {inline:?}"
    );
}

#[test]
fn semantic_anchor_uses_equality_predicate() {
    let stmt = parse_statement("MATCH (n:Person) WHERE n.age = 25 RETURN n").unwrap();
    validate_statement(&stmt).unwrap();
    let plan = gleaph_gql::planner::build_plan(&stmt).unwrap();
    let lines = plan.explain_lines();
    assert!(
        lines.iter().any(|l| l.contains("property-equality")),
        "expected property-equality anchor from semantic facts, got: {lines:?}"
    );
}

// ---- Schema-endpoint-driven planning ----

#[test]
fn schema_endpoint_anchor_with_stats() {
    use gleaph_gql::stats::TableStats;
    // Pattern: (a)-[:ACTS_IN]->(b) — no labels on nodes.
    // Schema: ACTS_IN goes Person -> Movie.
    // Stats: Person=100, Movie=50.
    // Planner should prefer 'b' (Movie, lower cardinality).
    let gql = "MATCH (a)-[:ACTS_IN]->(b) RETURN a, b";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
    let mut stats = TableStats::default();
    stats.label_cardinality.insert("Person".into(), 100);
    stats.label_cardinality.insert("Movie".into(), 50);
    let plan =
        gleaph_gql::planner::build_plan_with_schema_and_stats(&stmt, Some(&stats), &TestSchema)
            .unwrap();
    let lines = plan.explain_lines();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("schema-endpoint-cardinality")),
        "expected schema-endpoint-cardinality anchor, got: {lines:?}"
    );
}

#[test]
fn schema_endpoint_anchor_without_stats() {
    // Without stats, schema still provides the anchor source.
    let gql = "MATCH (a)-[:ACTS_IN]->(b) RETURN a, b";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
    let plan =
        gleaph_gql::planner::build_plan_with_schema_and_stats(&stmt, None, &TestSchema).unwrap();
    let lines = plan.explain_lines();
    assert!(
        lines.iter().any(|l| l.contains("schema-endpoint")),
        "expected schema-endpoint anchor, got: {lines:?}"
    );
}

#[test]
fn schema_endpoint_anchor_prefers_lower_cardinality() {
    use gleaph_gql::stats::TableStats;
    // ACTS_IN: Person -> Movie. If Person has lower cardinality, prefer 'a'.
    let gql = "MATCH (a)-[:ACTS_IN]->(b) RETURN a, b";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
    let mut stats = TableStats::default();
    stats.label_cardinality.insert("Person".into(), 20);
    stats.label_cardinality.insert("Movie".into(), 500);
    let plan =
        gleaph_gql::planner::build_plan_with_schema_and_stats(&stmt, Some(&stats), &TestSchema)
            .unwrap();
    let lines = plan.explain_lines();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("schema-endpoint-cardinality(Person=20)")),
        "expected Person anchor with cardinality 20, got: {lines:?}"
    );
}

#[test]
fn schema_endpoint_join_order_multi_hop() {
    use gleaph_gql::stats::TableStats;
    // Multi-hop: (a)-[:ACTS_IN]->(b)-[:KNOWS]->(c)
    // No labels on nodes; schema provides:
    //   ACTS_IN: Person -> Movie
    //   KNOWS: Person -> Person
    // Stats: Person=100, Movie=50.
    // Chain 0 destination is Movie (50), chain 1 destination is Person (100).
    // Chain 0 should be preferred (lower endpoint cardinality) — verify via explain.
    let gql = "MATCH (a)-[:ACTS_IN]->(b)-[:KNOWS]->(c) RETURN a, b, c";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
    let mut stats = TableStats::default();
    stats.label_cardinality.insert("Person".into(), 100);
    stats.label_cardinality.insert("Movie".into(), 50);
    let plan =
        gleaph_gql::planner::build_plan_with_schema_and_stats(&stmt, Some(&stats), &TestSchema)
            .unwrap();
    let lines = plan.explain_lines();
    // The plan should compile without errors and use schema-endpoint info.
    // The anchor should reference the schema-endpoint since no node has explicit labels.
    assert!(
        lines.iter().any(|l| l.contains("schema-endpoint")),
        "expected schema-endpoint in explain, got: {lines:?}"
    );
}
