//! Tests for the static type checking module.

use gleaph_gql::ast::{Keyword, ValueType};
use gleaph_gql::parser;
use gleaph_gql::type_check::schema::PropertySchema;
use gleaph_gql::type_check::{
    DML002_TARGET_VALUE, DML005_INSERT_EDGE_DIRECTION, DML006_MATCH_EDGE_DIRECTION,
    DiagnosticSeverity, NoSchema, TypeWarning, WarningKind, type_check, type_check_phase_b,
    type_check_strict, type_check_with_schema, type_diagnostic_from_warning,
};
use gleaph_gql::validate::validate;

// ── Helpers ──

fn parse_and_check(input: &str) -> Vec<TypeWarning> {
    let program = parser::parse(input).unwrap_or_else(|e| panic!("parse error: {e}"));
    validate(&program).unwrap_or_else(|e| panic!("validation error: {e}"));
    type_check(&program)
}

fn parse_and_check_with_schema(input: &str, schema: &dyn PropertySchema) -> Vec<TypeWarning> {
    let program = parser::parse(input).unwrap_or_else(|e| panic!("parse error: {e}"));
    validate(&program).unwrap_or_else(|e| panic!("validation error: {e}"));
    type_check_with_schema(&program, schema)
}

// ── Test schema ──

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
                (
                    "email".to_string(),
                    ValueType::String {
                        min_length: None,
                        max_length: None,
                    },
                    false,
                ),
            ]
        } else if labels.contains(&"Company".to_string()) {
            vec![
                (
                    "name".to_string(),
                    ValueType::String {
                        min_length: None,
                        max_length: None,
                    },
                    true,
                ),
                ("founded".to_string(), ValueType::Date, true),
            ]
        } else if labels.contains(&"Animal".to_string()) {
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
                    "species".to_string(),
                    ValueType::String {
                        min_length: None,
                        max_length: None,
                    },
                    true,
                ),
            ]
        } else {
            vec![]
        }
    }

    fn edge_property_types(&self, label: &str) -> Vec<(String, ValueType, bool)> {
        if label == "KNOWS" {
            vec![("since".to_string(), ValueType::Date, true)]
        } else {
            vec![]
        }
    }

    fn edge_endpoint_types(&self, label: &str) -> Vec<(Vec<String>, Vec<String>)> {
        if label == "KNOWS" {
            vec![(vec!["Person".to_string()], vec!["Person".to_string()])]
        } else {
            vec![]
        }
    }
}

// ── Basic tests ──

#[test]
fn no_warnings_simple_query() {
    let warnings = parse_and_check("MATCH (n) RETURN n");
    assert!(
        warnings.is_empty(),
        "expected no warnings, got: {warnings:?}"
    );
}

#[test]
fn no_warnings_with_literal_comparison() {
    let warnings = parse_and_check("MATCH (n) WHERE 1 = 1 RETURN n");
    assert!(
        warnings.is_empty(),
        "expected no warnings, got: {warnings:?}"
    );
}

// ── Arithmetic type mismatch ──

#[test]
fn binary_op_mismatch_string_plus_int() {
    let warnings =
        parse_and_check_with_schema("MATCH (n:Person) RETURN n.name + n.age", &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch warning, got: {warnings:?}"
    );
}

// ── Comparison type mismatch ──

#[test]
fn comparison_mismatch_int_vs_string() {
    let warnings = parse_and_check_with_schema(
        "MATCH (n:Person) WHERE n.age > n.name RETURN n",
        &TestSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::ComparisonMismatch),
        "expected ComparisonMismatch warning, got: {warnings:?}"
    );
}

#[test]
fn comparison_ok_same_types() {
    let warnings =
        parse_and_check_with_schema("MATCH (n:Person) WHERE n.age > 21 RETURN n", &TestSchema);
    // Should have no ComparisonMismatch — n.age (Int32) vs 21 (Int32)
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::ComparisonMismatch),
        "unexpected ComparisonMismatch: {warnings:?}"
    );
}

// ── IS NULL on NOT NULL ──

#[test]
fn null_check_on_nonnull_property() {
    let warnings = parse_and_check_with_schema(
        "MATCH (n:Person) WHERE n.name IS NULL RETURN n",
        &TestSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::NullCheckOnNonNull),
        "expected NullCheckOnNonNull warning, got: {warnings:?}"
    );
}

#[test]
fn null_check_on_nullable_property_no_warning() {
    let warnings = parse_and_check_with_schema(
        "MATCH (n:Person) WHERE n.email IS NULL RETURN n",
        &TestSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::NullCheckOnNonNull),
        "unexpected NullCheckOnNonNull: {warnings:?}"
    );
}

// ── Strict mode ──

#[test]
fn strict_mode_returns_error() {
    let program = parser::parse("MATCH (n:Person) RETURN n.name + n.age").unwrap();
    validate(&program).unwrap();
    let result = type_check_strict(&program, &TestSchema);
    assert!(result.is_err(), "expected error in strict mode");
}

#[test]
fn strict_mode_returns_ok_when_clean() {
    let program = parser::parse("MATCH (n) RETURN n").unwrap();
    validate(&program).unwrap();
    let result = type_check_strict(&program, &NoSchema);
    assert!(result.is_ok(), "expected Ok in strict mode for clean query");
}

// ── CASE / COALESCE ──

#[test]
fn no_warnings_coalesce() {
    let warnings = parse_and_check("MATCH (n) RETURN COALESCE(n.a, n.b)");
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

// ── Aggregate ──

#[test]
fn no_warnings_count_star() {
    let warnings = parse_and_check("MATCH (n) RETURN COUNT(*)");
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

#[test]
fn no_warnings_aggregate_functions() {
    let warnings = parse_and_check_with_schema(
        "MATCH (n:Person) RETURN COUNT(*), SUM(n.age), AVG(n.age), MIN(n.age), MAX(n.age)",
        &TestSchema,
    );
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

// ── CAST ──

#[test]
fn no_warnings_cast() {
    let warnings = parse_and_check("MATCH (n) RETURN CAST(n.x AS INT32)");
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

// ── Integration: parse → validate → type_check pipeline ──

#[test]
fn full_pipeline_complex_query() {
    let input = r#"
        MATCH (a:Person)-[e:KNOWS]->(b:Person)
        WHERE a.age > 18 AND b.age > 18
        RETURN a.name, b.name, e.since
    "#;
    let warnings = parse_and_check_with_schema(input, &TestSchema);
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

#[test]
fn full_pipeline_with_let() {
    let warnings = parse_and_check("MATCH (n) LET x = 42 RETURN n, x");
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

#[test]
fn full_pipeline_with_union() {
    let warnings = parse_and_check("MATCH (n) RETURN n UNION ALL MATCH (n) RETURN n");
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

// ── No warnings on DDL ──

#[test]
fn ddl_no_warnings() {
    let warnings = parse_and_check("CREATE SCHEMA /test");
    assert!(warnings.is_empty());
}

// ── Function arg mismatch ──

#[test]
#[cfg(feature = "cypher")]
fn function_arg_mismatch_id_on_edge() {
    let warnings = parse_and_check_with_schema(
        "MATCH (a:Person)-[e:KNOWS]->(b:Person) RETURN id(e)",
        &TestSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::FunctionArgMismatch),
        "expected FunctionArgMismatch for id(edge), got: {warnings:?}"
    );
}

#[test]
#[cfg(feature = "cypher")]
fn function_arg_ok_id_on_node() {
    let warnings = parse_and_check_with_schema("MATCH (n:Person) RETURN id(n)", &TestSchema);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::FunctionArgMismatch),
        "unexpected FunctionArgMismatch: {warnings:?}"
    );
}

// ── Type inference correctness ──

#[test]
fn infer_literal_types() {
    // Just ensure type_check runs without panics on literal-heavy queries.
    let warnings =
        parse_and_check("MATCH (n) RETURN 42, 3.14, 'hello', true, null, DATE '2024-01-01'");
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

// ── Filter with non-boolean ──

#[test]
fn filter_non_boolean_condition() {
    let warnings =
        parse_and_check_with_schema("MATCH (n:Person) FILTER n.age RETURN n", &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::NonBooleanCondition),
        "expected NonBooleanCondition, got: {warnings:?}"
    );
}

// ── OPTIONAL MATCH ──

#[test]
fn optional_match_no_nonnull_warning() {
    // IS NULL on a property from OPTIONAL MATCH should NOT warn
    // because the match itself might not have matched (whole row is null).
    let warnings = parse_and_check_with_schema(
        "MATCH (a:Person) OPTIONAL MATCH (b:Person) WHERE b.name IS NULL RETURN a",
        &TestSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::NullCheckOnNonNull),
        "unexpected NullCheckOnNonNull for OPTIONAL MATCH: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// Extended test schema with Company / WORKS_AT for endpoint constraint tests
// ════════════════════════════════════════════════════════════════════════════

struct ExtendedSchema;

impl PropertySchema for ExtendedSchema {
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
        } else if labels.contains(&"Company".to_string()) {
            vec![("founded".to_string(), ValueType::Date, true)]
        } else {
            vec![]
        }
    }

    fn edge_property_types(&self, label: &str) -> Vec<(String, ValueType, bool)> {
        match label {
            "KNOWS" => vec![("since".to_string(), ValueType::Date, true)],
            "WORKS_AT" => vec![(
                "role".to_string(),
                ValueType::String {
                    min_length: None,
                    max_length: None,
                },
                false,
            )],
            _ => vec![],
        }
    }

    fn edge_endpoint_types(&self, label: &str) -> Vec<(Vec<String>, Vec<String>)> {
        match label {
            // KNOWS: Person → Person only
            "KNOWS" => vec![(vec!["Person".to_string()], vec!["Person".to_string()])],
            // WORKS_AT: Person → Company only
            "WORKS_AT" => vec![(vec!["Person".to_string()], vec!["Company".to_string()])],
            _ => vec![],
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 1. Endpoint constraint tests (ImpossiblePattern)
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn endpoint_ok_person_knows_person() {
    let warnings = parse_and_check_with_schema(
        "MATCH (a:Person)-[e:KNOWS]->(b:Person) RETURN a, b",
        &ExtendedSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::ImpossiblePattern),
        "unexpected ImpossiblePattern: {warnings:?}"
    );
}

#[test]
fn endpoint_impossible_company_knows_person() {
    // Company → Person via KNOWS is not allowed by schema.
    let warnings = parse_and_check_with_schema(
        "MATCH (a:Company)-[e:KNOWS]->(b:Person) RETURN a, b",
        &ExtendedSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::ImpossiblePattern),
        "expected ImpossiblePattern for Company-KNOWS->Person, got: {warnings:?}"
    );
}

#[test]
fn endpoint_impossible_person_knows_company() {
    let warnings = parse_and_check_with_schema(
        "MATCH (a:Person)-[e:KNOWS]->(b:Company) RETURN a, b",
        &ExtendedSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::ImpossiblePattern),
        "expected ImpossiblePattern for Person-KNOWS->Company, got: {warnings:?}"
    );
}

#[test]
fn endpoint_undirected_ok_person_knows_person() {
    // Undirected: (Person)-[KNOWS]-(Person) → forward matches constraint, OK.
    let warnings = parse_and_check_with_schema(
        "MATCH (a:Person)-[e:KNOWS]-(b:Person) RETURN a, b",
        &ExtendedSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::ImpossiblePattern),
        "unexpected ImpossiblePattern: {warnings:?}"
    );
}

#[test]
fn endpoint_undirected_ok_works_at_either_direction() {
    // Undirected with tilde: (Company)~[WORKS_AT]~(Person) → reverse (Person→Company) matches.
    let warnings = parse_and_check_with_schema(
        "MATCH (a:Company)~[e:WORKS_AT]~(b:Person) RETURN a, b",
        &ExtendedSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::ImpossiblePattern),
        "unexpected ImpossiblePattern for undirected WORKS_AT: {warnings:?}"
    );
}

#[test]
fn endpoint_impossible_reverse_works_at() {
    // Directed: Company-[WORKS_AT]->Person is invalid (schema says Person→Company).
    let warnings = parse_and_check_with_schema(
        "MATCH (a:Company)-[e:WORKS_AT]->(b:Person) RETURN a, b",
        &ExtendedSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::ImpossiblePattern),
        "expected ImpossiblePattern for Company-WORKS_AT->Person, got: {warnings:?}"
    );
}

#[test]
fn endpoint_ok_person_works_at_company() {
    let warnings = parse_and_check_with_schema(
        "MATCH (a:Person)-[e:WORKS_AT]->(b:Company) RETURN a, b",
        &ExtendedSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::ImpossiblePattern),
        "unexpected ImpossiblePattern: {warnings:?}"
    );
}

#[test]
fn endpoint_no_labels_no_warning() {
    // No labels on nodes → cannot falsify, no warning expected.
    let warnings =
        parse_and_check_with_schema("MATCH (a)-[e:KNOWS]->(b) RETURN a, b", &ExtendedSchema);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::ImpossiblePattern),
        "unexpected ImpossiblePattern for unlabeled nodes: {warnings:?}"
    );
}

#[test]
fn endpoint_left_arrow_ok() {
    // <-[WORKS_AT]- from Company to Person means Person→Company in storage, which matches.
    let warnings = parse_and_check_with_schema(
        "MATCH (a:Company)<-[e:WORKS_AT]-(b:Person) RETURN a, b",
        &ExtendedSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::ImpossiblePattern),
        "unexpected ImpossiblePattern for left-arrow WORKS_AT: {warnings:?}"
    );
}

#[test]
fn complex_label_expr_does_not_infer_partial_schema_labels() {
    let warnings = parse_and_check_with_schema(
        "MATCH (n:(Person|Company)&Animal) WHERE n.species IS NULL RETURN n",
        &TestSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::NullCheckOnNonNull),
        "unexpected NullCheckOnNonNull from partial label extraction: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 2. Phase B constraint tests
// ════════════════════════════════════════════════════════════════════════════

fn parse_and_check_phase_b(input: &str, schema: &dyn PropertySchema) -> Vec<TypeWarning> {
    let program = parser::parse(input).unwrap_or_else(|e| panic!("parse error: {e}"));
    validate(&program).unwrap_or_else(|e| panic!("validation error: {e}"));
    type_check_phase_b(&program, schema)
}

#[test]
fn phase_b_detects_arithmetic_mismatch() {
    let warnings = parse_and_check_phase_b("MATCH (n:Person) RETURN n.name + n.age", &TestSchema);
    let phase_b_warnings: Vec<_> = warnings
        .iter()
        .filter(|w| w.message.starts_with("[phase-b]"))
        .collect();
    assert!(
        phase_b_warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected phase-b BinaryOpMismatch, got: {phase_b_warnings:?}"
    );
}

#[test]
fn phase_b_detects_comparison_mismatch() {
    let warnings = parse_and_check_phase_b(
        "MATCH (n:Person) WHERE n.age > n.name RETURN n",
        &TestSchema,
    );
    let phase_b_warnings: Vec<_> = warnings
        .iter()
        .filter(|w| w.message.starts_with("[phase-b]"))
        .collect();
    assert!(
        phase_b_warnings
            .iter()
            .any(|w| w.kind == WarningKind::ComparisonMismatch),
        "expected phase-b ComparisonMismatch, got: {phase_b_warnings:?}"
    );
}

#[test]
fn phase_b_detects_non_boolean_filter() {
    let warnings = parse_and_check_phase_b("MATCH (n:Person) FILTER n.age RETURN n", &TestSchema);
    let phase_b_warnings: Vec<_> = warnings
        .iter()
        .filter(|w| w.message.starts_with("[phase-b]"))
        .collect();
    assert!(
        phase_b_warnings
            .iter()
            .any(|w| w.kind == WarningKind::NonBooleanCondition),
        "expected phase-b NonBooleanCondition, got: {phase_b_warnings:?}"
    );
}

#[test]
fn phase_b_detects_null_check_on_nonnull() {
    let warnings = parse_and_check_phase_b(
        "MATCH (n:Person) WHERE n.name IS NULL RETURN n",
        &TestSchema,
    );
    let phase_b_warnings: Vec<_> = warnings
        .iter()
        .filter(|w| w.message.starts_with("[phase-b]"))
        .collect();
    assert!(
        phase_b_warnings
            .iter()
            .any(|w| w.kind == WarningKind::NullCheckOnNonNull),
        "expected phase-b NullCheckOnNonNull, got: {phase_b_warnings:?}"
    );
}

#[test]
fn phase_b_no_warnings_clean_query() {
    let warnings = parse_and_check_phase_b(
        "MATCH (a:Person)-[e:KNOWS]->(b:Person) WHERE a.age > 18 RETURN a.name, b.name",
        &TestSchema,
    );
    let phase_b_warnings: Vec<_> = warnings
        .iter()
        .filter(|w| w.message.starts_with("[phase-b]"))
        .collect();
    assert!(
        phase_b_warnings.is_empty(),
        "unexpected phase-b warnings: {phase_b_warnings:?}"
    );
}

#[test]
#[cfg(feature = "cypher")]
fn phase_b_detects_function_arg_mismatch() {
    let warnings = parse_and_check_phase_b(
        "MATCH (a:Person)-[e:KNOWS]->(b:Person) RETURN id(e)",
        &TestSchema,
    );
    let phase_b_warnings: Vec<_> = warnings
        .iter()
        .filter(|w| w.message.starts_with("[phase-b]"))
        .collect();
    assert!(
        phase_b_warnings
            .iter()
            .any(|w| w.kind == WarningKind::FunctionArgMismatch),
        "expected phase-b FunctionArgMismatch, got: {phase_b_warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 3. NEXT YIELD type propagation tests
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn next_yield_propagates_types() {
    // The second statement should know that `name` is String and `age` is Int32.
    // Comparing name > age should produce ComparisonMismatch.
    // Skip validation because the validator doesn't understand cross-NEXT bindings.
    let program = parser::parse(
        r#"
        MATCH (n:Person) RETURN n.name AS name, n.age AS age
        NEXT
        MATCH (m) WHERE name > age RETURN m
    "#,
    )
    .unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::ComparisonMismatch),
        "expected ComparisonMismatch from NEXT-propagated types, got: {warnings:?}"
    );
}

#[test]
fn next_yield_clean_usage() {
    // name is String in both statements; comparing strings is fine.
    // Skip validation because the validator doesn't understand cross-NEXT bindings.
    let program = parser::parse(
        r#"
        MATCH (n:Person) RETURN n.name AS name
        NEXT
        MATCH (m) WHERE name = 'Alice' RETURN m
    "#,
    )
    .unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::ComparisonMismatch),
        "unexpected ComparisonMismatch: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 4. Additional coverage: temporal, numeric promotion, CAST, CASE, etc.
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn temporal_addition_no_warning() {
    let warnings = parse_and_check("MATCH (n) RETURN DATE '2024-01-01' + DURATION 'P1D'");
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

#[test]
fn temporal_subtraction_produces_duration() {
    let warnings = parse_and_check("MATCH (n) RETURN DATE '2024-01-01' - DATE '2023-01-01'");
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

#[test]
fn cast_infers_result_type() {
    // CAST(x AS FLOAT64) + 'hello' should be a mismatch.
    let warnings = parse_and_check("MATCH (n) RETURN CAST(42 AS FLOAT64) + 'hello'");
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for FLOAT64 + STRING, got: {warnings:?}"
    );
}

#[test]
fn exists_subquery_no_warnings() {
    let warnings = parse_and_check_with_schema(
        "MATCH (n:Person) WHERE EXISTS { MATCH (n)-[:KNOWS]->(m:Person) } RETURN n",
        &TestSchema,
    );
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

#[test]
fn grouping_violation_detected() {
    // n.name is neither grouped nor aggregated — should warn.
    // Skip validation since the validator catches GROUP BY violations too.
    let program = parser::parse("MATCH (n:Person) RETURN n.name, COUNT(*) GROUP BY n.age").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::GroupingViolation),
        "expected GroupingViolation, got: {warnings:?}"
    );
}

#[test]
fn grouping_ok_when_grouped() {
    let warnings = parse_and_check_with_schema(
        "MATCH (n:Person) RETURN n.name, COUNT(*) GROUP BY n.name",
        &TestSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::GroupingViolation),
        "unexpected GroupingViolation: {warnings:?}"
    );
}

#[test]
fn narrowing_label_refines_properties() {
    // Within a single WHERE, the AND left side narrows n to Person, so the right
    // side sees n.name (String) and n.age (Int32) → ComparisonMismatch.
    let warnings = parse_and_check_with_schema(
        "MATCH (n) WHERE n IS LABELED Person AND n.name > n.age RETURN n",
        &ExtendedSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::ComparisonMismatch),
        "expected ComparisonMismatch after label narrowing, got: {warnings:?}"
    );
}

#[test]
#[cfg(feature = "cypher")]
fn edge_label_narrowing_resolves_properties() {
    // Within a single WHERE, the AND left side narrows e to KNOWS,
    // so the right side sees e.since (Date) > 'hello' (String) → ComparisonMismatch.
    let warnings = parse_and_check_with_schema(
        r#"MATCH (a)-[e]->(b) WHERE type(e) = 'KNOWS' AND e.since > 'hello' RETURN a, b"#,
        &ExtendedSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::ComparisonMismatch),
        "expected ComparisonMismatch for Date vs String after edge narrowing, got: {warnings:?}"
    );
}

#[test]
fn numeric_promotion_int8_plus_int64() {
    // Int literals are Int32, but adding to a cast Int64 should work fine.
    let warnings = parse_and_check("MATCH (n) RETURN CAST(1 AS INT8) + CAST(2 AS INT64)");
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

#[test]
fn list_concatenation_no_warning() {
    let warnings = parse_and_check("MATCH (n) RETURN [1, 2, 3] + [4, 5, 6]");
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

#[test]
fn string_concat_operator_no_warning() {
    let warnings = parse_and_check("MATCH (n) RETURN 'hello' || ' ' || 'world'");
    assert!(warnings.is_empty(), "got: {warnings:?}");
}

// ════════════════════════════════════════════════════════════════════════════
// 5. INSERT property type mismatch
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn insert_property_type_mismatch() {
    // Schema: Person.age is Int32, assigning a string literal should warn.
    let program = parser::parse("INSERT (n:Person {age: 'not a number'})").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::PropertyTypeMismatch),
        "expected PropertyTypeMismatch for String → Int32, got: {warnings:?}"
    );
}

#[test]
fn insert_property_type_ok() {
    // Schema: Person.age is Int32, assigning an int literal is fine.
    let program = parser::parse("INSERT (n:Person {age: 42})").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::PropertyTypeMismatch),
        "unexpected PropertyTypeMismatch: {warnings:?}"
    );
}

#[test]
fn insert_property_numeric_promotion_ok() {
    // Int8 assigned to Int32 property should be fine (numeric promotion).
    let program = parser::parse("INSERT (n:Person {age: CAST(1 AS INT8)})").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::PropertyTypeMismatch),
        "unexpected PropertyTypeMismatch for numeric promotion: {warnings:?}"
    );
}

#[test]
fn insert_unknown_label_no_warning() {
    // No label → no schema → open-world, should not warn.
    let program = parser::parse("INSERT (n {age: 'anything'})").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::PropertyTypeMismatch),
        "unexpected PropertyTypeMismatch for unlabeled node: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 6. SET property type mismatch
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn set_property_type_mismatch() {
    // SET n.age = 'hello' where n:Person → Int32 vs String mismatch.
    let program = parser::parse("MATCH (n:Person) SET n.age = 'hello'").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::PropertyTypeMismatch),
        "expected PropertyTypeMismatch for SET, got: {warnings:?}"
    );
}

#[test]
fn set_property_type_ok() {
    let program = parser::parse("MATCH (n:Person) SET n.age = 30").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::PropertyTypeMismatch),
        "unexpected PropertyTypeMismatch for SET: {warnings:?}"
    );
}

#[test]
fn set_scalar_target_warns() {
    let warnings = parse_and_check_with_schema(
        "MATCH (n:Person) LET x = n.age SET x.name = 'Bob'",
        &TestSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::DmlTargetMismatch),
        "expected DmlTargetMismatch for scalar SET, got: {warnings:?}"
    );
    assert!(
        warnings.iter().any(
            |w| w.kind == WarningKind::DmlTargetMismatch && w.code == Some(DML002_TARGET_VALUE)
        ),
        "expected DML002 code for scalar SET, got: {warnings:?}"
    );
}

#[test]
fn remove_scalar_target_warns() {
    let warnings =
        parse_and_check_with_schema("MATCH (n:Person) LET x = n.age REMOVE x.name", &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::DmlTargetMismatch),
        "expected DmlTargetMismatch for scalar REMOVE, got: {warnings:?}"
    );
    assert!(
        warnings.iter().any(
            |w| w.kind == WarningKind::DmlTargetMismatch && w.code == Some(DML002_TARGET_VALUE)
        ),
        "expected DML002 code for scalar REMOVE, got: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 7. Pattern-internal WHERE clause checking
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn pattern_internal_where_comparison_mismatch() {
    // WHERE inside node pattern: n.age > n.name should warn.
    let warnings = parse_and_check_with_schema(
        "MATCH (n:Person WHERE n.age > n.name) RETURN n",
        &TestSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::ComparisonMismatch),
        "expected ComparisonMismatch in pattern WHERE, got: {warnings:?}"
    );
}

#[test]
fn pattern_internal_where_clean() {
    let warnings =
        parse_and_check_with_schema("MATCH (n:Person WHERE n.age > 18) RETURN n", &TestSchema);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::ComparisonMismatch),
        "unexpected ComparisonMismatch: {warnings:?}"
    );
}

#[test]
fn pattern_internal_where_non_boolean() {
    let warnings =
        parse_and_check_with_schema("MATCH (n:Person WHERE n.age) RETURN n", &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::NonBooleanCondition),
        "expected NonBooleanCondition in pattern WHERE, got: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 8. UnaryOp validation (A)
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn unary_neg_on_string_warns() {
    let warnings = parse_and_check("MATCH (n) RETURN -'hello'");
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for unary neg on string, got: {warnings:?}"
    );
}

#[test]
fn unary_neg_on_int_ok() {
    let warnings = parse_and_check("MATCH (n) RETURN -42");
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 9. StringPredicate validation (B)
// ════════════════════════════════════════════════════════════════════════════

#[cfg(feature = "cypher")]
#[test]
fn string_predicate_non_string_warns() {
    let warnings = parse_and_check("MATCH (n) WHERE 'hello' STARTS WITH 42 RETURN n");
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::ComparisonMismatch),
        "expected ComparisonMismatch for STARTS WITH int, got: {warnings:?}"
    );
}

#[cfg(feature = "cypher")]
#[test]
fn string_predicate_strings_ok() {
    let warnings = parse_and_check("MATCH (n) WHERE 'hello' CONTAINS 'ell' RETURN n");
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::ComparisonMismatch),
        "unexpected ComparisonMismatch: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 10. Temporal arithmetic (C)
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn temporal_mul_warns() {
    // DATE * 2 should warn (temporal types don't support multiplication).
    let warnings = parse_and_check("MATCH (n) RETURN CURRENT_DATE * 2");
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for DATE * INT, got: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 11. CASE branch type mismatch (D)
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn case_branch_type_mismatch() {
    let warnings =
        parse_and_check("MATCH (n) RETURN CASE WHEN true THEN 42 WHEN false THEN 'hello' END");
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::CaseBranchTypeMismatch),
        "expected CaseBranchTypeMismatch, got: {warnings:?}"
    );
}

#[test]
fn case_branch_numeric_ok() {
    // Int and Float are compatible (numeric promotion).
    let warnings =
        parse_and_check("MATCH (n) RETURN CASE WHEN true THEN 1 WHEN false THEN 2.0 END");
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::CaseBranchTypeMismatch),
        "unexpected CaseBranchTypeMismatch: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 12. LetIn scoped inference (F)
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn let_in_type_mismatch() {
    // LET x = 'hi' IN x + 1 END should warn (String + Int).
    let warnings = parse_and_check("MATCH (n) RETURN LET x = 'hi' IN x + 1 END");
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch in LET-IN, got: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 13. GQL function arg type checks (G)
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn char_length_on_int_warns() {
    // CHAR_LENGTH is a dedicated AST node, not a FunctionCall.
    let warnings = parse_and_check("MATCH (n) RETURN CHAR_LENGTH(42)");
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::FunctionArgMismatch),
        "expected FunctionArgMismatch for CHAR_LENGTH(int), got: {warnings:?}"
    );
}

#[test]
fn char_length_on_string_ok() {
    let warnings = parse_and_check("MATCH (n) RETURN CHAR_LENGTH('hello')");
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::FunctionArgMismatch),
        "unexpected FunctionArgMismatch: {warnings:?}"
    );
}

#[test]
fn abs_on_string_warns() {
    let warnings = parse_and_check("MATCH (n) RETURN ABS('hello')");
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::FunctionArgMismatch),
        "expected FunctionArgMismatch for ABS(string), got: {warnings:?}"
    );
}

#[test]
fn abs_on_int_ok() {
    let warnings = parse_and_check("MATCH (n) RETURN ABS(-5)");
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::FunctionArgMismatch),
        "unexpected FunctionArgMismatch: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 14. OrderBy/Limit/Offset type checks (H)
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn limit_int_ok() {
    let warnings = parse_and_check("MATCH (n) RETURN n LIMIT 10");
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::NonNumericLimitOffset),
        "unexpected NonNumericLimitOffset: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 15. Phase B span propagation (J)
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn phase_b_warnings_have_span() {
    // Phase B should now include spans in its warnings.
    let program = parser::parse("MATCH (n:Person) WHERE n.age > 'old' RETURN n").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    let phase_b: Vec<_> = warnings.iter().filter(|w| w.provenance.is_some()).collect();
    for w in &phase_b {
        assert!(w.span.is_some(), "phase-b warning missing span: {w:?}");
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 16. Variable redefinition (K)
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn variable_redef_different_type_warns() {
    // Bind n as Person (Node), then try to use same var as Edge.
    let warnings =
        parse_and_check_with_schema("MATCH (n:Person), ()-[n:KNOWS]->() RETURN n", &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::VariableRedefinition),
        "expected VariableRedefinition, got: {warnings:?}"
    );
}

#[test]
fn variable_redef_same_type_no_warning() {
    // Rebinding n as Person in subquery should not warn.
    let warnings = parse_and_check_with_schema(
        "MATCH (n:Person) WHERE EXISTS { MATCH (n)-[:KNOWS]->(m:Person) } RETURN n",
        &TestSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::VariableRedefinition),
        "unexpected VariableRedefinition: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 17. Concat (||) non-string validation
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn concat_int_warns() {
    let warnings = parse_and_check("MATCH (n) RETURN 42 || true");
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for non-string concat, got: {warnings:?}"
    );
}

#[test]
fn concat_strings_ok() {
    let warnings = parse_and_check("MATCH (n) RETURN 'hello' || ' world'");
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch for string concat: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 18. FOR loop element type inference
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn for_element_type_inferred() {
    // FOR x IN [1, 2, 3] — x should be Int, so x + 'hi' should warn.
    let warnings = parse_and_check("FOR x IN [1, 2, 3] RETURN x + 'hello'");
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for Int + String in FOR, got: {warnings:?}"
    );
}

#[test]
fn for_element_type_compatible() {
    let warnings = parse_and_check("FOR x IN [1, 2, 3] RETURN x + 1");
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 19. INSERT required property missing
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn insert_missing_required_property() {
    // Person has `name` (required) and `age` (required). Omitting name should warn.
    let program = parser::parse("INSERT (n:Person {age: 42})").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::MissingRequiredProperty),
        "expected MissingRequiredProperty, got: {warnings:?}"
    );
}

#[test]
fn insert_all_required_properties_ok() {
    let program = parser::parse("INSERT (n:Person {name: 'Alice', age: 30})").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::MissingRequiredProperty),
        "unexpected MissingRequiredProperty: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 20. ValueSubquery type inference
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn value_subquery_infers_type() {
    // VALUE { MATCH (n:Person) RETURN n.age } should infer Int32,
    // so adding a string should warn.
    let warnings = parse_and_check_with_schema(
        "MATCH (x) RETURN VALUE { MATCH (n:Person) RETURN n.age } + 'hi'",
        &TestSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for Int + String from VALUE subquery, got: {warnings:?}"
    );
}

#[test]
fn value_subquery_compatible_ok() {
    let warnings = parse_and_check_with_schema(
        "MATCH (x) RETURN VALUE { MATCH (n:Person) RETURN n.age } + 1",
        &TestSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 21. CallProcedure type checking
// ════════════════════════════════════════════════════════════════════════════

use gleaph_gql::type_check::ProcedureSignature;

/// Test schema with procedure support.
struct TestSchemaWithProcs;

impl PropertySchema for TestSchemaWithProcs {
    fn node_property_types(&self, labels: &[String]) -> Vec<(String, ValueType, bool)> {
        TestSchema.node_property_types(labels)
    }
    fn edge_property_types(&self, label: &str) -> Vec<(String, ValueType, bool)> {
        TestSchema.edge_property_types(label)
    }
    fn edge_endpoint_types(&self, label: &str) -> Vec<(Vec<String>, Vec<String>)> {
        TestSchema.edge_endpoint_types(label)
    }
    fn procedure_signature(&self, name: &str) -> Option<ProcedureSignature> {
        match name {
            "db.stats" => Some(ProcedureSignature {
                params: vec![],
                yields: vec![
                    (
                        "nodeCount".into(),
                        ValueType::Int64 {
                            keyword: Keyword::new("INT64"),
                        },
                    ),
                    (
                        "edgeCount".into(),
                        ValueType::Int64 {
                            keyword: Keyword::new("INT64"),
                        },
                    ),
                ],
            }),
            "math.add" => Some(ProcedureSignature {
                params: vec![
                    (
                        "a".into(),
                        ValueType::Int64 {
                            keyword: Keyword::new("INT64"),
                        },
                    ),
                    (
                        "b".into(),
                        ValueType::Int64 {
                            keyword: Keyword::new("INT64"),
                        },
                    ),
                ],
                yields: vec![(
                    "result".into(),
                    ValueType::Int64 {
                        keyword: Keyword::new("INT64"),
                    },
                )],
            }),
            _ => None,
        }
    }
}

#[test]
fn call_procedure_arg_count_mismatch() {
    // math.add expects 2 args, providing 1 should warn.
    let program = parser::parse("CALL math.add(1) YIELD result RETURN result").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchemaWithProcs);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::FunctionArgMismatch),
        "expected FunctionArgMismatch for wrong arg count, got: {warnings:?}"
    );
}

#[test]
fn call_procedure_arg_type_mismatch() {
    // math.add expects Int64, providing string should warn.
    let program = parser::parse("CALL math.add('hi', 2) YIELD result RETURN result").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchemaWithProcs);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::FunctionArgMismatch),
        "expected FunctionArgMismatch for wrong arg type, got: {warnings:?}"
    );
}

#[test]
fn call_procedure_yield_binds_types() {
    // db.stats YIELD nodeCount → Int64, so nodeCount + 'hi' should warn.
    let program =
        parser::parse("CALL db.stats() YIELD nodeCount RETURN nodeCount + 'hello'").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchemaWithProcs);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for Int64 + String from YIELD, got: {warnings:?}"
    );
}

#[test]
fn call_procedure_clean() {
    let program = parser::parse("CALL db.stats() YIELD nodeCount RETURN nodeCount + 1").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchemaWithProcs);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 22. SUM return type promotion
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn sum_int_promotes_to_int64() {
    // SUM(n.age) where age is Int32 → Int64, so comparing with a string should warn.
    let warnings =
        parse_and_check_with_schema("MATCH (n:Person) RETURN SUM(n.age) + 'hello'", &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for SUM(Int32→Int64) + String, got: {warnings:?}"
    );
}

#[test]
fn sum_int_plus_int_ok() {
    let warnings =
        parse_and_check_with_schema("MATCH (n:Person) RETURN SUM(n.age) + 1", &TestSchema);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 23. ListIndex element type inference
// ════════════════════════════════════════════════════════════════════════════

#[cfg(feature = "cypher")]
#[test]
fn list_index_infers_element_type() {
    // xs[0] where xs: List<Int64> should infer Int64, so adding string warns.
    let warnings = parse_and_check("MATCH (n) LET xs = [1, 2, 3] RETURN xs[0] + 'hi'");
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for list[idx] + String, got: {warnings:?}"
    );
}

#[cfg(feature = "cypher")]
#[test]
fn list_index_compatible_ok() {
    let warnings = parse_and_check("MATCH (n) LET xs = [1, 2, 3] RETURN xs[0] + 10");
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 24. DML target type check
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn delete_node_ok() {
    let warnings = parse_and_check_with_schema("MATCH (n:Person) DELETE n", &TestSchema);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::DmlTargetMismatch),
        "unexpected DmlTargetMismatch: {warnings:?}"
    );
}

#[test]
fn delete_scalar_warns() {
    let warnings =
        parse_and_check_with_schema("MATCH (n:Person) LET x = n.age DELETE x", &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::DmlTargetMismatch),
        "expected DmlTargetMismatch for scalar DELETE, got: {warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| {
            w.kind == WarningKind::DmlTargetMismatch && w.code == Some(DML002_TARGET_VALUE)
        }),
        "expected DML002 code for scalar DELETE, got: {warnings:?}"
    );
    let diagnostic = type_diagnostic_from_warning(
        warnings
            .iter()
            .find(|w| w.kind == WarningKind::DmlTargetMismatch)
            .expect("expected DML warning"),
    );
    assert_eq!(diagnostic.code, Some(DML002_TARGET_VALUE));
    assert_eq!(diagnostic.severity, DiagnosticSeverity::Warning);
}

#[test]
fn delete_edge_ok() {
    let warnings = parse_and_check_with_schema(
        "MATCH (a:Person)-[e:KNOWS]->(b:Person) DELETE e",
        &TestSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::DmlTargetMismatch),
        "unexpected DmlTargetMismatch: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 25. UNION/EXCEPT/INTERSECT column type compatibility
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn union_compatible_columns_ok() {
    let warnings = parse_and_check_with_schema(
        "MATCH (n:Person) RETURN n.age UNION MATCH (m:Person) RETURN m.age",
        &TestSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::SetOpColumnMismatch),
        "unexpected SetOpColumnMismatch: {warnings:?}"
    );
}

#[test]
fn union_incompatible_columns_warns() {
    // n.name is String, m.age is Int32 → incompatible.
    let warnings = parse_and_check_with_schema(
        "MATCH (n:Person) RETURN n.name UNION MATCH (m:Person) RETURN m.age",
        &TestSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::SetOpColumnMismatch),
        "expected SetOpColumnMismatch for String vs Int, got: {warnings:?}"
    );
}

#[test]
fn union_numeric_promotion_ok() {
    // Int32 UNION Int64 → both numeric, compatible.
    let warnings = parse_and_check("MATCH (n) RETURN 1 UNION MATCH (m) RETURN CAST(2 AS INT8)");
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::SetOpColumnMismatch),
        "unexpected SetOpColumnMismatch for numeric promotion: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 26. Record field access type inference
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn record_field_access_infers_type() {
    // {name: 'Alice', age: 30}.age is Int64, adding string should warn.
    let warnings =
        parse_and_check("MATCH (n) LET r = {name: 'Alice', age: 30} RETURN r.age + 'hi'");
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for record field Int + String, got: {warnings:?}"
    );
}

#[test]
fn record_field_access_ok() {
    let warnings = parse_and_check("MATCH (n) LET r = {name: 'Alice', age: 30} RETURN r.age + 1");
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 27. RETURN * propagation via NEXT YIELD
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn return_star_next_yield_propagates_types() {
    // RETURN * should propagate n:Person bindings, so n.age (Int32) + 'hi' warns.
    // Skip validation since the scope checker doesn't expand RETURN *.
    let program =
        parser::parse("MATCH (n:Person) RETURN * NEXT YIELD n MATCH (m) RETURN n.age + 'hi'")
            .unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch after RETURN * NEXT YIELD, got: {warnings:?}"
    );
}

#[test]
fn return_star_next_yield_clean() {
    let program =
        parser::parse("MATCH (n:Person) RETURN * NEXT YIELD n MATCH (m) RETURN n.age + 1").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch: {warnings:?}"
    );
}

#[test]
fn next_yield_hides_non_projected_bindings() {
    let program = parser::parse(
        "MATCH (n:Person)-[:KNOWS]->(m:Person) RETURN n, m NEXT YIELD n RETURN m + 1",
    )
    .unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch from leaked NEXT scope: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 28. Group variables in quantified patterns
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn quantified_parenthesized_node_becomes_list() {
    // Inside ((n:Person)-[e:KNOWS]->(m:Person)){2,5}, n/m/e should be List types.
    // So n + 1 should warn (List + Int).
    let program =
        parser::parse("MATCH ((n:Person)-[e:KNOWS]->(m:Person)){2,5} RETURN n + 1").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for List<Node> + Int, got: {warnings:?}"
    );
}

#[test]
fn non_quantified_parenthesized_node_stays_node() {
    // Without quantifier, (n:Person) is just Node.
    let warnings = parse_and_check_with_schema(
        "MATCH ((n:Person)-[e:KNOWS]->(m:Person)) RETURN n.age + 1",
        &TestSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch for non-quantified pattern: {warnings:?}"
    );
}

#[test]
fn quantified_edge_without_parens_is_path() {
    // -[e:KNOWS]->{2,5} without parens → Path, not List<Edge>.
    let program = parser::parse("MATCH (a:Person)-[e:KNOWS]->{2,5}(b:Person) RETURN e").unwrap();
    let warnings = type_check_with_schema(&program, &TestSchema);
    // e is Path — no mismatch expected for just returning it.
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected warning: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 29. OR narrowing and multi-label property intersection
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn or_narrowing_shared_property_same_type() {
    // Both Person and Company have `name: String`.
    // After OR narrowing, n.name should resolve to String → adding int should warn.
    let warnings = parse_and_check_with_schema(
        "MATCH (n) WHERE n IS LABELED Person OR n IS LABELED Company RETURN n.name + 1",
        &TestSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for String + Int after OR narrowing, got: {warnings:?}"
    );
}

#[test]
fn or_narrowing_shared_property_ok() {
    // n.name is String in both Person and Company → String concat is fine.
    let warnings = parse_and_check_with_schema(
        "MATCH (n) WHERE n IS LABELED Person OR n IS LABELED Company RETURN n.name || ' suffix'",
        &TestSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch: {warnings:?}"
    );
}

#[test]
fn or_narrowing_disjoint_property_unknown() {
    // Person has `age`, Company does not.
    // After OR narrowing, n.age should be Unknown → no BinaryOpMismatch (Unknown suppresses).
    let warnings = parse_and_check_with_schema(
        "MATCH (n) WHERE n IS LABELED Person OR n IS LABELED Company RETURN n.age + 'hi'",
        &TestSchema,
    );
    // n.age is Unknown because Company doesn't have it → no warning.
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch for Unknown property: {warnings:?}"
    );
}

#[test]
fn and_then_or_narrowing() {
    // AND + OR: n.active IS NOT NULL AND (n IS LABELED Person OR n IS LABELED Company)
    // After narrowing, n.name should resolve to String.
    let warnings = parse_and_check_with_schema(
        "MATCH (n) WHERE n.email IS NOT NULL AND (n IS LABELED Person OR n IS LABELED Company) RETURN n.name + 1",
        &TestSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for String + Int in AND+OR, got: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 30. Nested OR narrowing (3+ branches)
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn nested_or_three_labels_shared_property() {
    // (Person OR Company) OR Animal — all three have `name: String`.
    // n.name should resolve to String → adding int warns.
    let warnings = parse_and_check_with_schema(
        "MATCH (n) WHERE (n IS LABELED Person OR n IS LABELED Company) OR n IS LABELED Animal RETURN n.name + 1",
        &TestSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for String + Int with 3-way OR, got: {warnings:?}"
    );
}

#[test]
fn nested_or_three_labels_disjoint_property() {
    // Person has `age`, Company and Animal don't.
    // n.age should be Unknown after 3-way OR.
    let warnings = parse_and_check_with_schema(
        "MATCH (n) WHERE (n IS LABELED Person OR n IS LABELED Company) OR n IS LABELED Animal RETURN n.age + 'hi'",
        &TestSchema,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "unexpected BinaryOpMismatch — n.age should be Unknown with 3-way OR: {warnings:?}"
    );
}

#[test]
fn nested_or_right_associative() {
    // Person OR (Company OR Animal) — right-associative grouping.
    // All have `name: String` → String + Int warns.
    let warnings = parse_and_check_with_schema(
        "MATCH (n) WHERE n IS LABELED Person OR (n IS LABELED Company OR n IS LABELED Animal) RETURN n.name + 1",
        &TestSchema,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == WarningKind::BinaryOpMismatch),
        "expected BinaryOpMismatch for String + Int with right-assoc 3-way OR, got: {warnings:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// Schema edge direction (UNDIRECTED EDGE / DIRECTED EDGE vs pattern syntax)
// ════════════════════════════════════════════════════════════════════════════

struct UndirectedKnowsSchema;

impl PropertySchema for UndirectedKnowsSchema {
    fn node_property_types(&self, _labels: &[String]) -> Vec<(String, ValueType, bool)> {
        vec![]
    }

    fn edge_property_types(&self, _label: &str) -> Vec<(String, ValueType, bool)> {
        vec![]
    }

    fn edge_is_undirected(&self, label: &str) -> Option<bool> {
        (label == "KNOWS").then_some(true)
    }
}

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

#[test]
fn insert_directed_arrow_conflicts_undirected_schema() {
    let warnings = parse_and_check_with_schema("INSERT (a)-[:KNOWS]->(b)", &UndirectedKnowsSchema);
    assert!(
        warnings.iter().any(|w| {
            w.kind == WarningKind::SchemaEdgeDirectionMismatch
                && w.code == Some(DML005_INSERT_EDGE_DIRECTION)
        }),
        "expected schema edge direction fatal for INSERT, got: {warnings:?}"
    );
}

#[test]
fn insert_undirected_syntax_conflicts_directed_schema() {
    let warnings = parse_and_check_with_schema("INSERT (a)~[:KNOWS]~(b)", &DirectedKnowsSchema);
    assert!(
        warnings.iter().any(|w| {
            w.kind == WarningKind::SchemaEdgeDirectionMismatch
                && w.code == Some(DML005_INSERT_EDGE_DIRECTION)
        }),
        "expected schema edge direction fatal for INSERT tilde, got: {warnings:?}"
    );
}

#[test]
fn match_directed_arrow_warns_on_undirected_schema() {
    let warnings =
        parse_and_check_with_schema("MATCH (a)-[:KNOWS]->(b) RETURN a", &UndirectedKnowsSchema);
    assert!(
        warnings.iter().any(|w| {
            w.kind == WarningKind::SchemaEdgeDirectionMismatch
                && w.code == Some(DML006_MATCH_EDGE_DIRECTION)
        }),
        "expected MATCH schema edge direction warning, got: {warnings:?}"
    );
}
