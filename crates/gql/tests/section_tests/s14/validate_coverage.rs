//! §14 — Validation coverage (access modes, constraints).

use crate::section_tests::p;
use gleaph_gql::validate::validate;

// ── Validate.rs coverage ────────────────────────────────────────────────

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
    let prog =
        p("MATCH (n) RETURN CASE WHEN n.x > 0 THEN 'pos' ELSE 'neg' END, COUNT(*) GROUP BY n.x");
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
    let prog =
        p("MATCH (n) RETURN CASE n.x WHEN 1 THEN 'one' ELSE 'other' END, COUNT(*) GROUP BY n.x");
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
