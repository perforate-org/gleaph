//! Tests that parse the GQL reference grammar sample files.
//! These are temporary tests to verify parser coverage of standard GQL samples.

use gleaph_gql::parser;
use gleaph_gql::validate::validate;

fn try_parse(input: &str) -> Result<gleaph_gql::ast::GqlProgram, gleaph_gql::GqlError> {
    parser::parse(input)
}

fn parse_ok(input: &str) -> gleaph_gql::ast::GqlProgram {
    match try_parse(input) {
        Ok(prog) => {
            validate(&prog).unwrap_or_else(|_| panic!("validation failed for: {input}"));
            prog
        }
        Err(e) => panic!("parse failed for: {input}\nerror: {e}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// create_schema.gql
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn sample_create_schema() {
    parse_ok("CREATE SCHEMA /myschema");
}

#[test]
fn sample_create_schema_nested_path() {
    parse_ok("CREATE SCHEMA /foo/myschema");
}

#[test]
fn sample_create_schema_next() {
    parse_ok("CREATE SCHEMA /foo NEXT CREATE SCHEMA /fee");
}

// ═══════════════════════════════════════════════════════════════════════════
// create_graph.gql
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn sample_create_graph_any() {
    parse_ok("CREATE GRAPH mygraph ANY");
}

#[test]
fn sample_create_graph_inline_type() {
    parse_ok(
        "CREATE GRAPH mygraph { \
         (Person :Person {lastname STRING, firstname STRING, joined DATE}) \
         }",
    );
}

#[test]
fn sample_create_graph_named_type() {
    parse_ok("CREATE GRAPH mygraph mygraphtype");
}

#[test]
fn sample_create_graph_like() {
    parse_ok("CREATE GRAPH /mygraph LIKE /mysrcgraph");
}

#[test]
fn sample_create_graph_any_as_copy_of() {
    parse_ok("CREATE GRAPH mygraph ANY AS COPY OF mysrcgraph");
}

#[test]
fn sample_create_graph_inline_as_copy_of() {
    parse_ok(
        "CREATE GRAPH mygraph { \
         (Person :Person {lastname STRING, firstname STRING, joined DATE}) \
         } AS COPY OF mysrcgraph",
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// create_closed_graph_from_graph_type_*.gql
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn sample_create_graph_typed() {
    parse_ok("CREATE GRAPH mySocialNetwork TYPED socialNetworkGraphType");
}

#[test]
fn sample_create_graph_double_colon() {
    parse_ok("CREATE GRAPH mySocialNetwork ::socialNetworkGraphType");
}

#[test]
fn sample_create_graph_nested_type_double_colon() {
    parse_ok(
        "CREATE GRAPH mySocialNetwork ::\
         {(City :City {name STRING, state STRING, country STRING})}",
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// insert_statement.gql
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn sample_insert_with_date_literal() {
    parse_ok(
        "INSERT (:Person { firstname: 'Firstname', lastname: 'Lastname', joined: DATE '2023-01-01' }) \
         -[:MEMBER_SINCE { since: '2023-03-20' }]-> \
         (:Team { name: 'Teamname' })",
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// match_and_insert_example.gql
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn sample_match_and_insert() {
    parse_ok(
        "MATCH (a { firstname: 'Robert' }), (b { lastname: 'Kowalski' }) \
         INSERT (a)-[:GRADUATED]->(b)",
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// match_with_exists_predicate_*.gql
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn sample_exists_with_braces() {
    parse_ok(
        "MATCH (p:Person)-[r:IS_FRIENDS_WITH]->(friend:Person) \
         WHERE EXISTS { MATCH (p)-[:WORKS_FOR]->(:Company { name: 'GQL, Inc.' }) RETURN p } \
         RETURN p, r, friend",
    );
}

// EXISTS (MATCH ...) with parentheses is no longer supported —
// GQL §20.13 uses braces only.

// ═══════════════════════════════════════════════════════════════════════════
// session_*.gql
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn sample_session_set_graph_current_graph() {
    parse_ok("SESSION SET GRAPH CURRENT_GRAPH");
}

#[test]
fn sample_session_set_graph_current_property_graph() {
    parse_ok("SESSION SET GRAPH CURRENT_PROPERTY_GRAPH");
}

#[test]
fn sample_session_set_value_if_not_exists() {
    parse_ok("SESSION SET VALUE IF NOT EXISTS $exampleProperty = DATE '2022-10-10'");
}

#[test]
fn sample_session_set_time_zone() {
    parse_ok("SESSION SET TIME ZONE 'utc'");
}
