use gleaph_gql::{executor::collect_param_names_from_stmt, parse_statement, validate_statement};
use std::collections::BTreeSet;

fn param_names(gql: &str) -> BTreeSet<String> {
    let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    let mut out = BTreeSet::new();
    collect_param_names_from_stmt(&stmt, &mut out);
    out
}

/// Helper that skips validation (for testing param collection on statements
/// that validation rejects, e.g. DELETE/SET without hops).
fn param_names_no_validate(gql: &str) -> BTreeSet<String> {
    let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
    let mut out = BTreeSet::new();
    collect_param_names_from_stmt(&stmt, &mut out);
    out
}

#[test]
fn collect_params_from_query() {
    let names = param_names("MATCH (n:User) WHERE n.name = $name RETURN n.name");
    assert_eq!(names, BTreeSet::from(["name".into()]));
}

#[test]
fn collect_params_from_create() {
    let names = param_names("MERGE (n:User {name: 'test'}) ON CREATE SET n.age = $age");
    assert_eq!(names, BTreeSet::from(["age".into()]));
}

#[test]
fn collect_params_from_set() {
    let names =
        param_names("MATCH (a:User)-[:KNOWS]->(b) WHERE a.name = $name SET b.score = $score");
    assert_eq!(names, BTreeSet::from(["name".into(), "score".into()]));
}

#[test]
fn collect_params_from_delete() {
    // DELETE requires at least 1 hop for validation, so use no-validate helper
    // to test the param walker on a simpler pattern.
    let names = param_names_no_validate("MATCH (n:User) WHERE n.name = $name DELETE n");
    assert_eq!(names, BTreeSet::from(["name".into()]));
}

#[test]
fn collect_params_from_merge() {
    // MERGE property hints use parse_value_expr which doesn't support $param,
    // so params come from ON CREATE/MATCH SET clauses.
    let names = param_names("MERGE (n:User {name: 'test'}) ON CREATE SET n.score = $score");
    assert_eq!(names, BTreeSet::from(["score".into()]));
}

#[test]
fn no_params_returns_empty() {
    let names = param_names("MATCH (n) RETURN n LIMIT 1");
    assert!(names.is_empty());
}

#[test]
fn collect_params_from_compound() {
    let names = param_names(
        "MATCH (a) WHERE a.x = $p1 RETURN a.x UNION MATCH (b) WHERE b.y = $p2 RETURN b.y",
    );
    assert_eq!(names, BTreeSet::from(["p1".into(), "p2".into()]));
}
