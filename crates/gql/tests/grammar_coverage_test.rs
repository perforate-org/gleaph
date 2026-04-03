//! GQL grammar coverage tests.
//!
//! This test module systematically verifies that the gql crate can parse
//! representative inputs for every grammar section in GQL (ISO/IEC 39075).
//! Each test is annotated with the corresponding GQL section number.
//!
//! If a grammar rule is added to GQL and is not covered here, the gap is
//! immediately visible.

use gleaph_gql::parser;
use gleaph_gql::validate::validate;

/// Parse and validate — panics with a useful message on failure.
fn ok(input: &str) {
    let program =
        parser::parse(input).unwrap_or_else(|e| panic!("parse failed: {e}\ninput: {input}"));
    validate(&program).unwrap_or_else(|e| panic!("validate failed: {e}\ninput: {input}"));
}

/// Parse only (skip validation) — for statements whose validation rules may
/// reject valid-syntax inputs (e.g. unbound variables in contrived examples).
fn ok_syntax(input: &str) {
    parser::parse(input).unwrap_or_else(|e| panic!("parse failed: {e}\ninput: {input}"));
}

// ════════════════════════════════════════════════════════════════════════════════
// §6 — GQL program structure
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn s06_bare_query() {
    ok("MATCH (n) RETURN n");
}

#[test]
fn s06_session_then_query() {
    ok_syntax("SESSION SET SCHEMA /mydb SESSION SET GRAPH myGraph MATCH (n) RETURN n");
}

#[test]
fn s06_session_close() {
    ok_syntax("MATCH (n) RETURN n SESSION CLOSE");
}

// ════════════════════════════════════════════════════════════════════════════════
// §7 — Session commands
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn s07_01_session_set_schema() {
    ok_syntax("SESSION SET SCHEMA /mydb");
}

#[test]
fn s07_01_session_set_graph() {
    ok_syntax("SESSION SET GRAPH myGraph");
}

#[test]
fn s07_01_session_set_property_graph() {
    ok_syntax("SESSION SET PROPERTY GRAPH myGraph");
}

#[test]
fn s07_01_session_set_timezone() {
    ok_syntax("SESSION SET TIME ZONE 'UTC'");
}

#[test]
fn s07_01_session_set_value_param() {
    ok_syntax("SESSION SET VALUE $x = 42");
}

#[test]
fn s07_01_session_set_graph_param() {
    ok_syntax("SESSION SET GRAPH $g = myGraph");
}

#[test]
fn s07_01_session_set_table_param() {
    ok_syntax("SESSION SET TABLE $t = $other");
}

#[test]
fn s07_02_session_reset_all() {
    ok_syntax("SESSION RESET ALL PARAMETERS");
}

#[test]
fn s07_02_session_reset_schema() {
    ok_syntax("SESSION RESET SCHEMA");
}

#[test]
fn s07_02_session_reset_graph() {
    ok_syntax("SESSION RESET GRAPH");
}

#[test]
fn s07_02_session_reset_timezone() {
    ok_syntax("SESSION RESET TIME ZONE");
}

#[test]
fn s07_02_session_reset_parameters() {
    ok_syntax("SESSION RESET PARAMETERS");
}

#[test]
fn s07_02_session_reset_all_characteristics() {
    ok_syntax("SESSION RESET ALL CHARACTERISTICS");
}

#[test]
fn s07_02_session_reset_characteristics() {
    ok_syntax("SESSION RESET CHARACTERISTICS");
}

#[test]
fn s07_02_session_reset_property_graph() {
    ok_syntax("SESSION RESET PROPERTY GRAPH");
}

#[test]
fn s07_02_session_reset_parameter_named() {
    ok_syntax("SESSION RESET PARAMETER $x");
}

#[test]
fn s07_02_session_reset_bare_param() {
    ok_syntax("SESSION RESET $x");
}

#[test]
fn s07_03_session_close_standalone() {
    ok_syntax("SESSION CLOSE");
}

// ════════════════════════════════════════════════════════════════════════════════
// §8 — Transaction commands
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn s08_01_start_transaction() {
    ok_syntax("START TRANSACTION");
}

#[test]
fn s08_02_start_transaction_read_only() {
    ok_syntax("START TRANSACTION READ ONLY");
}

#[test]
fn s08_02_start_transaction_read_write() {
    ok_syntax("START TRANSACTION READ WRITE");
}

#[test]
fn s08_03_rollback() {
    ok_syntax("ROLLBACK");
}

#[test]
fn s08_04_commit() {
    ok_syntax("COMMIT");
}

#[test]
fn s08_transaction_with_body() {
    ok_syntax("START TRANSACTION MATCH (n) RETURN n COMMIT");
}

// ════════════════════════════════════════════════════════════════════════════════
// §9 — Nested procedure specification / procedure body
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn s09_01_nested_procedure() {
    ok_syntax("CALL { MATCH (n) RETURN n }");
}

#[test]
fn s09_02_procedure_body_with_bindings() {
    ok_syntax("VALUE x = 1 MATCH (n) WHERE n.id = x RETURN n");
}

#[test]
fn s09_02_procedure_body_with_graph_binding() {
    ok_syntax("GRAPH g = myGraph MATCH (n) RETURN n");
}

// ════════════════════════════════════════════════════════════════════════════════
// §10 — Variable definitions (graph, binding table, value)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn s10_01_graph_variable_def() {
    ok_syntax("GRAPH g = myGraph MATCH (n) RETURN n");
}

#[test]
fn s10_01_graph_variable_def_typed_any() {
    ok_syntax("GRAPH g :: ANY GRAPH = myGraph MATCH (n) RETURN n");
}

#[test]
fn s10_01_graph_variable_def_typed_closed() {
    ok_syntax("GRAPH g TYPED GRAPH myType = myGraph MATCH (n) RETURN n");
}

#[test]
fn s10_02_binding_table_variable_def() {
    ok_syntax("TABLE t = { MATCH (n) RETURN n } MATCH (m) RETURN m");
}

#[test]
fn s10_03_value_variable_def() {
    ok_syntax("VALUE x = 42 MATCH (n) WHERE n.id = x RETURN n");
}

#[test]
fn s10_03_value_variable_def_typed() {
    ok_syntax("VALUE x :: INT32 = 42 MATCH (n) WHERE n.id = x RETURN n");
}

// ════════════════════════════════════════════════════════════════════════════════
// §11 — Graph / binding table expressions
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn s11_01_current_graph() {
    ok_syntax("USE CURRENT_GRAPH MATCH (n) RETURN n");
}

#[test]
fn s11_01_current_property_graph() {
    ok_syntax("USE CURRENT_PROPERTY_GRAPH MATCH (n) RETURN n");
}

#[test]
fn s11_01_home_graph() {
    ok_syntax("USE HOME_GRAPH MATCH (n) RETURN n");
}

#[test]
fn s11_01_home_property_graph() {
    ok_syntax("USE HOME_PROPERTY_GRAPH MATCH (n) RETURN n");
}

// ════════════════════════════════════════════════════════════════════════════════
// §12 — Catalog-modifying statements (CREATE / DROP)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn s12_02_create_schema() {
    ok_syntax("CREATE SCHEMA /mydb");
}

#[test]
fn s12_02_create_schema_if_not_exists() {
    ok_syntax("CREATE SCHEMA IF NOT EXISTS /mydb");
}

#[test]
fn s12_03_drop_schema() {
    ok_syntax("DROP SCHEMA /mydb");
}

#[test]
fn s12_03_drop_schema_if_exists() {
    ok_syntax("DROP SCHEMA IF EXISTS /mydb");
}

#[test]
fn s12_04_create_graph() {
    ok_syntax("CREATE GRAPH myGraph {}");
}

#[test]
fn s12_04_create_graph_if_not_exists() {
    ok_syntax("CREATE GRAPH IF NOT EXISTS myGraph {}");
}

#[test]
fn s12_04_create_or_replace_graph() {
    ok_syntax("CREATE OR REPLACE GRAPH myGraph {}");
}

#[test]
fn s12_04_create_graph_open_type() {
    ok_syntax("CREATE GRAPH myGraph ANY");
}

#[test]
fn s12_04_create_graph_typed() {
    ok_syntax("CREATE GRAPH myGraph TYPED myType");
}

#[test]
fn s12_04_create_graph_typed_double_colon() {
    ok_syntax("CREATE GRAPH myGraph :: myType");
}

#[test]
fn s12_04_create_graph_like() {
    ok_syntax("CREATE GRAPH myGraph LIKE otherGraph");
}

#[test]
fn s12_04_create_graph_copy_of() {
    ok_syntax("CREATE GRAPH myGraph {} AS COPY OF otherGraph");
}

#[test]
fn s12_05_drop_graph() {
    ok_syntax("DROP GRAPH myGraph");
}

#[test]
fn s12_05_drop_graph_if_exists() {
    ok_syntax("DROP GRAPH IF EXISTS myGraph");
}

#[test]
fn s12_06_create_graph_type() {
    ok_syntax("CREATE GRAPH TYPE myType {}");
}

#[test]
fn s12_06_create_graph_type_if_not_exists() {
    ok_syntax("CREATE GRAPH TYPE IF NOT EXISTS myType {}");
}

#[test]
fn s12_06_create_or_replace_graph_type() {
    ok_syntax("CREATE OR REPLACE GRAPH TYPE myType {}");
}

#[test]
fn s12_06_create_graph_type_copy_of() {
    ok_syntax("CREATE GRAPH TYPE myType COPY OF otherType {}");
}

#[test]
fn s12_07_drop_graph_type() {
    ok_syntax("DROP GRAPH TYPE myType");
}

#[test]
fn s12_07_drop_graph_type_if_exists() {
    ok_syntax("DROP GRAPH TYPE IF EXISTS myType");
}

// ════════════════════════════════════════════════════════════════════════════════
// §13 — Data-modifying statements
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn s13_02_insert_node() {
    ok_syntax("INSERT (n:Person {name: 'Alice'})");
}

#[test]
fn s13_02_insert_edge() {
    ok_syntax("INSERT (a)-[:KNOWS]->(b)");
}

#[test]
fn s13_02_insert_path() {
    ok_syntax("INSERT (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})");
}

#[test]
fn s13_03_set_property() {
    ok_syntax("MATCH (n) SET n.name = 'Bob' RETURN n");
}

#[test]
fn s13_03_set_all_properties() {
    ok_syntax("MATCH (n) SET n = {name: 'Bob', age: 30} RETURN n");
}

#[test]
fn s13_03_set_label() {
    ok_syntax("MATCH (n) SET n :Admin RETURN n");
}

#[test]
fn s13_03_set_label_is() {
    ok_syntax("MATCH (n) SET n IS Admin RETURN n");
}

#[test]
fn s13_04_remove_property() {
    ok_syntax("MATCH (n) REMOVE n.name RETURN n");
}

#[test]
fn s13_04_remove_label() {
    ok_syntax("MATCH (n) REMOVE n :Admin RETURN n");
}

#[test]
fn s13_04_remove_label_is() {
    ok_syntax("MATCH (n) REMOVE n IS Admin RETURN n");
}

#[test]
fn s13_05_delete() {
    ok("MATCH (n) DELETE n");
}

#[test]
fn s13_05_detach_delete() {
    ok("MATCH (n) DETACH DELETE n");
}

#[test]
fn s13_05_nodetach_delete() {
    ok("MATCH (n) NODETACH DELETE n");
}

#[test]
fn s13_05_delete_expression() {
    ok_syntax("MATCH (n) DELETE n.prop");
}

#[test]
fn s13_05_delete_multiple() {
    ok("MATCH (a)-[e]->(b) DELETE a, e, b");
}

// ════════════════════════════════════════════════════════════════════════════════
// §14 — Query statements
// ════════════════════════════════════════════════════════════════════════════════

// §14.1 — composite query statement
#[test]
fn s14_01_composite_query() {
    ok("MATCH (n:A) RETURN n AS x UNION MATCH (n:B) RETURN n AS x");
}

// §14.2 — composite query expression (set operators)
#[test]
fn s14_02_union_all() {
    ok("MATCH (n:A) RETURN n AS x UNION ALL MATCH (n:B) RETURN n AS x");
}

#[test]
fn s14_02_union_distinct() {
    ok("MATCH (n:A) RETURN n AS x UNION DISTINCT MATCH (n:B) RETURN n AS x");
}

#[test]
fn s14_02_except() {
    ok("MATCH (n:A) RETURN n AS x EXCEPT MATCH (n:B) RETURN n AS x");
}

#[test]
fn s14_02_except_all() {
    ok("MATCH (n:A) RETURN n AS x EXCEPT ALL MATCH (n:B) RETURN n AS x");
}

#[test]
fn s14_02_intersect() {
    ok("MATCH (n:A) RETURN n AS x INTERSECT MATCH (n:B) RETURN n AS x");
}

#[test]
fn s14_02_intersect_all() {
    ok("MATCH (n:A) RETURN n AS x INTERSECT ALL MATCH (n:B) RETURN n AS x");
}

#[test]
fn s14_02_otherwise() {
    ok("MATCH (n:A) RETURN n AS x OTHERWISE MATCH (n:B) RETURN n AS x");
}

// §14.3 — linear query statement
#[test]
fn s14_03_linear_query_multiple_parts() {
    ok("MATCH (a) MATCH (a)-[e]->(b) RETURN a, b");
}

// §14.4 — MATCH statement
#[test]
fn s14_04_match() {
    ok("MATCH (n:Person) RETURN n");
}

#[test]
fn s14_04_optional_match() {
    ok("MATCH (n) OPTIONAL MATCH (n)-[e]->(m) RETURN n, m");
}

#[test]
fn s14_04_match_yield() {
    ok_syntax("MATCH (n) YIELD n RETURN n");
}

// §14.5 — CALL query statement
#[test]
fn s14_05_call_named() {
    ok_syntax("CALL myProc() RETURN *");
}

#[test]
fn s14_05_call_with_args() {
    ok_syntax("CALL myProc(1, 'test') YIELD x RETURN x");
}

#[test]
fn s14_05_optional_call() {
    ok_syntax("OPTIONAL CALL myProc() YIELD x RETURN x");
}

// §14.6 — FILTER statement
#[test]
fn s14_06_filter() {
    ok("MATCH (n) FILTER n.age > 30 RETURN n");
}

#[test]
fn s14_06_filter_where() {
    ok("MATCH (n) FILTER WHERE n.age > 30 RETURN n");
}

// §14.7 — LET statement
#[test]
fn s14_07_let() {
    ok("MATCH (n) LET x = n.age RETURN x");
}

#[test]
fn s14_07_let_multiple() {
    ok("MATCH (n) LET x = n.age, y = n.name RETURN x, y");
}

// §14.8 — FOR statement
#[test]
fn s14_08_for() {
    ok_syntax("MATCH (n) FOR x IN n.items RETURN x");
}

#[test]
fn s14_08_for_with_ordinality() {
    ok_syntax("MATCH (n) FOR x IN n.items WITH ORDINALITY i RETURN x, i");
}

#[test]
fn s14_08_for_with_offset() {
    ok_syntax("MATCH (n) FOR x IN n.items WITH OFFSET i RETURN x, i");
}

// §14.9 — ORDER BY and page
#[test]
fn s14_09_order_by() {
    ok("MATCH (n) RETURN n ORDER BY n.name");
}

#[test]
fn s14_09_order_by_desc() {
    ok("MATCH (n) RETURN n ORDER BY n.name DESC");
}

#[test]
fn s14_09_offset() {
    ok("MATCH (n) RETURN n OFFSET 5");
}

#[test]
fn s14_09_limit() {
    ok("MATCH (n) RETURN n LIMIT 10");
}

#[test]
fn s14_09_order_by_offset_limit() {
    ok("MATCH (n) RETURN n ORDER BY n.name OFFSET 5 LIMIT 10");
}

#[test]
fn s14_09_limit_offset_reversed() {
    ok("MATCH (n) RETURN n LIMIT 10 OFFSET 5");
}

// §14.10 — Primitive result statement
#[test]
fn s14_10_return() {
    ok("MATCH (n) RETURN n");
}

#[test]
fn s14_10_finish() {
    ok_syntax("MATCH (n) FINISH");
}

// §14.11 — RETURN statement
#[test]
fn s14_11_return_star() {
    ok("MATCH (n) RETURN *");
}

#[test]
fn s14_11_return_distinct() {
    ok("MATCH (n) RETURN DISTINCT n");
}

#[test]
fn s14_11_return_alias() {
    ok("MATCH (n) RETURN n.name AS name");
}

#[test]
fn s14_11_return_group_by() {
    ok("MATCH (n) RETURN n.label, COUNT(*) AS cnt GROUP BY n.label");
}

#[cfg(feature = "cypher")]
#[test]
fn s14_11_return_no_bindings() {
    ok_syntax("MATCH (n) RETURN NO BINDINGS");
}

// §14.12 — SELECT statement
#[test]
fn s14_12_select_star() {
    ok_syntax("SELECT * FROM myGraph MATCH (n)");
}

#[test]
fn s14_12_select_items() {
    ok_syntax("SELECT n.name AS name FROM myGraph MATCH (n)");
}

#[test]
fn s14_12_select_distinct() {
    ok_syntax("SELECT DISTINCT n.label FROM myGraph MATCH (n)");
}

#[test]
fn s14_12_select_having() {
    ok_syntax(
        "SELECT n.label, COUNT(*) AS cnt FROM myGraph MATCH (n) GROUP BY n.label HAVING cnt > 5",
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// §15 — CALL procedure
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn s15_02_inline_call() {
    ok_syntax("CALL { MATCH (n) RETURN n }");
}

#[test]
fn s15_02_inline_call_with_scope() {
    ok_syntax("MATCH (a) CALL (a) { MATCH (a)-[e]->(b) RETURN b } RETURN a");
}

#[test]
fn s15_02_optional_inline_call() {
    ok_syntax("OPTIONAL CALL { MATCH (n) RETURN n }");
}

#[test]
fn s15_03_named_call() {
    ok_syntax("CALL myProc(1, 'hello')");
}

#[test]
fn s15_03_named_call_yield() {
    ok_syntax("CALL myProc() YIELD x, y RETURN x");
}

// ════════════════════════════════════════════════════════════════════════════════
// §16 — Graph pattern
// ════════════════════════════════════════════════════════════════════════════════

// §16.1 — AT schema clause
#[test]
fn s16_01_at_schema() {
    ok_syntax("AT /mydb MATCH (n) RETURN n");
}

// §16.2 — USE graph clause
#[test]
fn s16_02_use_graph() {
    ok_syntax("USE myGraph MATCH (n) RETURN n");
}

#[test]
fn s16_02_use_graph_nested() {
    ok_syntax("USE myGraph { MATCH (n) RETURN n }");
}

// §16.4 — Graph pattern (match modes)
#[test]
fn s16_04_match_mode_repeatable_elements() {
    ok_syntax("MATCH REPEATABLE ELEMENTS (n)-[e]->(m) RETURN n");
}

#[test]
fn s16_04_match_mode_different_edges() {
    ok_syntax("MATCH DIFFERENT EDGES (n)-[e]->(m) RETURN n");
}

#[test]
fn s16_04_keep_clause() {
    ok_syntax("MATCH (n)-[e]->(m) KEEP TRAIL RETURN n");
}

// §16.5 — Insert graph pattern
#[test]
fn s16_05_insert_undirected_edge() {
    ok_syntax("INSERT (a)~[:KNOWS]~(b)");
}

#[test]
fn s16_05_insert_left_edge() {
    ok_syntax("INSERT (a)<-[:KNOWS]-(b)");
}

// §16.6 — Path pattern prefix (path modes and search prefixes)
#[test]
fn s16_06_walk() {
    ok_syntax("MATCH WALK (n)-[e]->(m) RETURN n");
}

#[test]
fn s16_06_trail() {
    ok_syntax("MATCH TRAIL (n)-[e]->(m) RETURN n");
}

#[test]
fn s16_06_simple() {
    ok_syntax("MATCH SIMPLE (n)-[e]->(m) RETURN n");
}

#[test]
fn s16_06_acyclic() {
    ok_syntax("MATCH ACYCLIC (n)-[e]->(m) RETURN n");
}

#[test]
fn s16_06_any_path() {
    ok_syntax("MATCH ANY (n)-[e]->(m) RETURN n");
}

#[test]
fn s16_06_any_shortest() {
    ok_syntax("MATCH ANY SHORTEST (a)-[e]->{1,5}(b) RETURN a, b");
}

#[test]
fn s16_06_all_shortest() {
    ok_syntax("MATCH ALL SHORTEST (a)-[e]->{1,5}(b) RETURN a, b");
}

#[test]
fn s16_06_shortest_n() {
    ok_syntax("MATCH SHORTEST 3 (a)-[e]->{1,5}(b) RETURN a, b");
}

#[test]
fn s16_06_shortest_group() {
    ok_syntax("MATCH SHORTEST 2 PATHS GROUP (a)-[e]->{1,5}(b) RETURN a, b");
}

// §16.7 — Path pattern expression
#[test]
fn s16_07_node_pattern() {
    ok("MATCH (n) RETURN n");
}

#[test]
fn s16_07_edge_pattern_right() {
    ok("MATCH (a)-[e]->(b) RETURN a");
}

#[test]
fn s16_07_edge_pattern_left() {
    ok("MATCH (a)<-[e]-(b) RETURN a");
}

#[test]
fn s16_07_edge_pattern_undirected() {
    ok("MATCH (a)~[e]~(b) RETURN a");
}

#[test]
fn s16_07_edge_pattern_any_direction() {
    ok("MATCH (a)-[e]-(b) RETURN a");
}

#[test]
fn s16_07_edge_pattern_left_or_undirected() {
    ok_syntax("MATCH (a)<~[e]~(b) RETURN a");
}

#[test]
fn s16_07_edge_pattern_undirected_or_right() {
    ok_syntax("MATCH (a)~[e]~>(b) RETURN a");
}

#[test]
fn s16_07_edge_pattern_left_or_right() {
    ok_syntax("MATCH (a)<-[e]->(b) RETURN a");
}

#[test]
fn s16_07_parenthesized_path() {
    ok("MATCH ((a)-[e]->(b)) RETURN a");
}

#[test]
fn s16_07_parenthesized_path_with_where() {
    ok("MATCH ((a)-[e]->(b) WHERE a.x > b.x) RETURN a");
}

#[test]
fn s16_07_path_union() {
    ok_syntax("MATCH (a)(()-[:A]->() | ()-[:B]->())(b) RETURN a, b");
}

#[test]
fn s16_07_multiset_alternation() {
    ok_syntax("MATCH (a)(()-[:A]->() |+| ()-[:B]->())(b) RETURN a, b");
}

// §16.8 — Label expression
#[test]
fn s16_08_label_name() {
    ok("MATCH (n:Person) RETURN n");
}

#[test]
fn s16_08_label_conjunction() {
    ok("MATCH (n:Person&Employee) RETURN n");
}

#[test]
fn s16_08_label_disjunction() {
    ok("MATCH (n:Person|Company) RETURN n");
}

#[test]
fn s16_08_label_negation() {
    ok("MATCH (n:!Person) RETURN n");
}

#[test]
fn s16_08_label_wildcard() {
    ok("MATCH (n:%) RETURN n");
}

#[test]
fn s16_08_label_parenthesized() {
    ok("MATCH (n:(Person|Company)&Active) RETURN n");
}

// §16.11 — Graph pattern quantifier
#[test]
fn s16_11_quantifier_star() {
    ok_syntax("MATCH (a)-[e]->{0,}(b) RETURN a");
}

#[test]
fn s16_11_quantifier_plus() {
    ok_syntax("MATCH (a)-[e]->{1,}(b) RETURN a");
}

#[test]
fn s16_11_quantifier_question() {
    ok_syntax("MATCH (a)-[e]->?(b) RETURN a");
}

#[test]
fn s16_11_quantifier_fixed() {
    ok_syntax("MATCH (a)-[e]->{3}(b) RETURN a");
}

#[test]
fn s16_11_quantifier_range() {
    ok_syntax("MATCH (a)-[e]->{2,5}(b) RETURN a");
}

#[test]
fn s16_11_quantifier_lower_only() {
    ok_syntax("MATCH (a)-[e]->{2,}(b) RETURN a");
}

// §16.12 — Simplified path pattern expression
#[test]
fn s16_12_simplified_right() {
    ok_syntax("MATCH (a)-/KNOWS/->(b) RETURN a, b");
}

#[test]
fn s16_12_simplified_left() {
    ok_syntax("MATCH (a)<-/KNOWS/-(b) RETURN a, b");
}

#[test]
fn s16_12_simplified_undirected() {
    ok_syntax("MATCH (a)~/KNOWS/~(b) RETURN a, b");
}

#[test]
fn s16_12_simplified_any_direction() {
    ok_syntax("MATCH (a)-/KNOWS/-(b) RETURN a, b");
}

// §16.12 — Simplified direction overrides (inside simplified path contents)
#[test]
fn s16_12_override_left() {
    ok_syntax("MATCH (a)-/<KNOWS/->(b) RETURN a, b");
}

#[test]
fn s16_12_override_right() {
    ok_syntax("MATCH (a)-/KNOWS>/->(b) RETURN a, b");
}

#[test]
fn s16_12_override_undirected() {
    ok_syntax("MATCH (a)-/~KNOWS/->(b) RETURN a, b");
}

#[test]
fn s16_12_override_left_or_undirected() {
    ok_syntax("MATCH (a)-/<~KNOWS/->(b) RETURN a, b");
}

#[test]
fn s16_12_override_undirected_or_right() {
    ok_syntax("MATCH (a)-/~KNOWS>/->(b) RETURN a, b");
}

#[test]
fn s16_12_override_left_or_right() {
    ok_syntax("MATCH (a)-/<KNOWS>/->(b) RETURN a, b");
}

#[test]
fn s16_12_override_any_direction() {
    ok_syntax("MATCH (a)-/-KNOWS/->(b) RETURN a, b");
}

// §16.12 — Simplified path: negation and wildcard
#[test]
fn s16_12_simplified_negation() {
    ok_syntax("MATCH (a)-/!KNOWS/->(b) RETURN a, b");
}

#[test]
fn s16_12_simplified_wildcard() {
    ok_syntax("MATCH (a)-/%/->(b) RETURN a, b");
}

#[test]
fn s16_12_simplified_conjunction() {
    ok_syntax("MATCH (a)-/KNOWS&LIKES/->(b) RETURN a, b");
}

#[test]
fn s16_12_simplified_union() {
    ok_syntax("MATCH (a)-/KNOWS|LIKES/->(b) RETURN a, b");
}

#[test]
fn s16_12_simplified_group() {
    ok_syntax("MATCH (a)-/(KNOWS|LIKES)/->(b) RETURN a, b");
}

#[test]
fn s16_12_simplified_quantified() {
    ok_syntax("MATCH (a)-/KNOWS{2,5}/->(b) RETURN a, b");
}

// §16.13 — WHERE clause
#[test]
fn s16_13_where() {
    ok("MATCH (n) WHERE n.age > 30 RETURN n");
}

// §16.14 — YIELD clause
#[test]
fn s16_14_yield() {
    ok_syntax("MATCH (n) YIELD n RETURN n");
}

// §16.15 — GROUP BY clause
#[test]
fn s16_15_group_by() {
    ok("MATCH (n) RETURN n.label, COUNT(*) AS cnt GROUP BY n.label");
}

// §16.16 — ORDER BY clause
#[test]
fn s16_16_order_by_multiple() {
    ok("MATCH (n) RETURN n ORDER BY n.name ASC, n.age DESC");
}

// §16.17 — Sort specification (NULLS FIRST / LAST)
#[test]
fn s16_17_sort_nulls_first() {
    ok("MATCH (n) RETURN n ORDER BY n.name NULLS FIRST");
}

#[test]
fn s16_17_sort_nulls_last() {
    ok("MATCH (n) RETURN n ORDER BY n.name NULLS LAST");
}

// §16.18 — LIMIT clause
#[test]
fn s16_18_limit() {
    ok("MATCH (n) RETURN n LIMIT 10");
}

// §16.19 — OFFSET clause
#[test]
fn s16_19_offset() {
    ok("MATCH (n) RETURN n OFFSET 5");
}

// ════════════════════════════════════════════════════════════════════════════════
// §17 — References (schema, graph, graph type, binding table, procedure)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn s17_01_schema_reference_absolute() {
    ok_syntax("AT /mydb MATCH (n) RETURN n");
}

#[test]
fn s17_01_schema_reference_current() {
    ok_syntax("AT CURRENT_SCHEMA MATCH (n) RETURN n");
}

#[test]
fn s17_01_schema_reference_home() {
    ok_syntax("AT HOME_SCHEMA MATCH (n) RETURN n");
}

#[test]
fn s17_02_graph_reference_qualified() {
    ok_syntax("USE schema1.myGraph MATCH (n) RETURN n");
}

#[test]
fn s17_02_graph_reference_absolute() {
    ok_syntax("USE /catalog/myGraph MATCH (n) RETURN n");
}

#[test]
fn s17_05_procedure_reference_qualified() {
    ok_syntax("CALL schema1.myProc()");
}

// ════════════════════════════════════════════════════════════════════════════════
// §18 — Graph type specification & value types
// ════════════════════════════════════════════════════════════════════════════════

// §18.1 — Nested graph type specification
#[test]
fn s18_01_graph_type_inline() {
    ok_syntax(
        "CREATE GRAPH myGraph { NODE Person LABEL Person { name STRING }, DIRECTED EDGE Knows LABEL Knows CONNECTING (Person -> Person) }",
    );
}

#[test]
fn s18_01_graph_type_pattern_syntax() {
    ok_syntax("CREATE GRAPH myGraph { (:Person {name STRING})-[:KNOWS]->(:Person) }");
}

// §18.9 — Value types: Boolean
#[test]
fn s18_09_type_bool() {
    ok_syntax("VALUE x :: BOOL = TRUE MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_boolean() {
    ok_syntax("VALUE x :: BOOLEAN = TRUE MATCH (n) RETURN n");
}

// §18.9 — Value types: String
#[test]
fn s18_09_type_string() {
    ok_syntax("VALUE x :: STRING = 'hello' MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_string_max() {
    ok_syntax("VALUE x :: STRING(100) = 'hello' MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_string_min_max() {
    ok_syntax("VALUE x :: STRING(1, 100) = 'hello' MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_char() {
    ok_syntax("VALUE x :: CHAR(10) = 'hello' MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_varchar() {
    ok_syntax("VALUE x :: VARCHAR(255) = 'hello' MATCH (n) RETURN n");
}

// §18.9 — Value types: Byte strings
#[test]
fn s18_09_type_bytes() {
    ok_syntax("VALUE x :: BYTES = X'FF' MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_binary() {
    ok_syntax("VALUE x :: BINARY(16) = X'FF' MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_varbinary() {
    ok_syntax("VALUE x :: VARBINARY(256) = X'FF' MATCH (n) RETURN n");
}

// §18.9 — Value types: Signed integers
#[test]
fn s18_09_type_int8() {
    ok_syntax("VALUE x :: INT8 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_int16() {
    ok_syntax("VALUE x :: INT16 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_int32() {
    ok_syntax("VALUE x :: INT32 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_int64() {
    ok_syntax("VALUE x :: INT64 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_int128() {
    ok_syntax("VALUE x :: INT128 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_int256() {
    ok_syntax("VALUE x :: INT256 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_smallint() {
    ok_syntax("VALUE x :: SMALLINT = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_bigint() {
    ok_syntax("VALUE x :: BIGINT = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_int_precision() {
    ok_syntax("VALUE x :: INT(10) = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_integer_precision() {
    ok_syntax("VALUE x :: INTEGER(10) = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_signed_integer() {
    ok_syntax("VALUE x :: SIGNED INTEGER = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_signed_integer8() {
    ok_syntax("VALUE x :: SIGNED INTEGER8 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_signed_small_integer() {
    ok_syntax("VALUE x :: SIGNED SMALL INTEGER = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_signed_big_integer() {
    ok_syntax("VALUE x :: SIGNED BIG INTEGER = 1 MATCH (n) RETURN n");
}

// §18.9 — Value types: Unsigned integers
#[test]
fn s18_09_type_uint8() {
    ok_syntax("VALUE x :: UINT8 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_uint16() {
    ok_syntax("VALUE x :: UINT16 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_uint32() {
    ok_syntax("VALUE x :: UINT32 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_uint64() {
    ok_syntax("VALUE x :: UINT64 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_uint128() {
    ok_syntax("VALUE x :: UINT128 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_uint256() {
    ok_syntax("VALUE x :: UINT256 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_usmallint() {
    ok_syntax("VALUE x :: USMALLINT = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_ubigint() {
    ok_syntax("VALUE x :: UBIGINT = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_uint_precision() {
    ok_syntax("VALUE x :: UINT(10) = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_unsigned_integer() {
    ok_syntax("VALUE x :: UNSIGNED INTEGER = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_unsigned_integer16() {
    ok_syntax("VALUE x :: UNSIGNED INTEGER16 = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_unsigned_small_integer() {
    ok_syntax("VALUE x :: UNSIGNED SMALL INTEGER = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_unsigned_big_integer() {
    ok_syntax("VALUE x :: UNSIGNED BIG INTEGER = 1 MATCH (n) RETURN n");
}

// §18.9 — Value types: Decimal
#[test]
fn s18_09_type_decimal() {
    ok_syntax("VALUE x :: DECIMAL = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_decimal_precision() {
    ok_syntax("VALUE x :: DECIMAL(10) = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_decimal_precision_scale() {
    ok_syntax("VALUE x :: DECIMAL(10, 2) = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_dec() {
    ok_syntax("VALUE x :: DEC = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_numeric() {
    ok_syntax("VALUE x :: NUMERIC(10, 2) = 1 MATCH (n) RETURN n");
}

// §18.9 — Value types: Float
#[test]
fn s18_09_type_float16() {
    ok_syntax("VALUE x :: FLOAT16 = 1.0 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_float32() {
    ok_syntax("VALUE x :: FLOAT32 = 1.0 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_float64() {
    ok_syntax("VALUE x :: FLOAT64 = 1.0 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_float128() {
    ok_syntax("VALUE x :: FLOAT128 = 1.0 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_float256() {
    ok_syntax("VALUE x :: FLOAT256 = 1.0 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_float_precision() {
    ok_syntax("VALUE x :: FLOAT(24) = 1.0 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_float_precision_scale() {
    ok_syntax("VALUE x :: FLOAT(24, 5) = 1.0 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_real() {
    ok_syntax("VALUE x :: REAL = 1.0 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_double() {
    ok_syntax("VALUE x :: DOUBLE = 1.0 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_double_precision() {
    ok_syntax("VALUE x :: DOUBLE PRECISION = 1.0 MATCH (n) RETURN n");
}

// §18.9 — Value types: Temporal
#[test]
fn s18_09_type_date() {
    ok_syntax("VALUE x :: DATE = DATE '2024-01-01' MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_zoned_datetime() {
    ok_syntax("VALUE x :: ZONED DATETIME = DATETIME '2024-01-01T00:00:00Z' MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_zoned_time() {
    ok_syntax("VALUE x :: ZONED TIME = TIME '12:00:00+00:00' MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_local_datetime() {
    ok_syntax("VALUE x :: LOCAL DATETIME = DATETIME '2024-01-01T00:00:00' MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_local_time() {
    ok_syntax("VALUE x :: LOCAL TIME = TIME '12:00:00' MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_timestamp_with_tz() {
    ok_syntax(
        "VALUE x :: TIMESTAMP WITH TIME ZONE = DATETIME '2024-01-01T00:00:00Z' MATCH (n) RETURN n",
    );
}

#[test]
fn s18_09_type_timestamp_without_tz() {
    ok_syntax(
        "VALUE x :: TIMESTAMP WITHOUT TIME ZONE = DATETIME '2024-01-01T00:00:00' MATCH (n) RETURN n",
    );
}

#[test]
fn s18_09_type_timestamp_bare() {
    ok_syntax("VALUE x :: TIMESTAMP = DATETIME '2024-01-01T00:00:00' MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_duration_year_to_month() {
    ok_syntax("VALUE x :: DURATION(YEAR TO MONTH) = DURATION 'P1Y2M' MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_duration_day_to_second() {
    ok_syntax("VALUE x :: DURATION(DAY TO SECOND) = DURATION 'P1DT2H' MATCH (n) RETURN n");
}

// §18.9 — Value types: Reference types
#[test]
fn s18_09_type_any_graph() {
    ok_syntax("VALUE x :: ANY GRAPH = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_any_property_graph() {
    ok_syntax("VALUE x :: ANY PROPERTY GRAPH = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_node() {
    ok_syntax("VALUE x :: NODE = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_any_node() {
    ok_syntax("VALUE x :: ANY NODE = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_edge() {
    ok_syntax("VALUE x :: EDGE = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_any_edge() {
    ok_syntax("VALUE x :: ANY EDGE = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_binding_table() {
    ok_syntax("VALUE x :: BINDING TABLE = 1 MATCH (n) RETURN n");
}

// §18.9 — Value types: Path
#[test]
fn s18_09_type_path() {
    ok_syntax("VALUE x :: PATH = 1 MATCH (n) RETURN n");
}

// §18.9 — Value types: List / Array
#[test]
fn s18_09_type_list_prefix() {
    ok_syntax("VALUE x :: LIST<INT32> = [1] MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_list_postfix() {
    ok_syntax("VALUE x :: INT32 LIST = [1] MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_array_prefix() {
    ok_syntax("VALUE x :: ARRAY<STRING> = ['a'] MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_list_with_max_length() {
    ok_syntax("VALUE x :: LIST<INT32>[10] = [1] MATCH (n) RETURN n");
}

// §18.9 — Value types: Record
#[test]
fn s18_09_type_record() {
    ok_syntax(
        "VALUE x :: RECORD {name STRING, age INT32} = {name: 'A', age: 1} MATCH (n) RETURN n",
    );
}

#[test]
fn s18_09_type_any_record() {
    ok_syntax("VALUE x :: ANY RECORD = {a: 1} MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_record_bare_braces() {
    ok_syntax("VALUE x :: {name STRING} = {name: 'A'} MATCH (n) RETURN n");
}

// §18.9 — Value types: Dynamic union
#[test]
fn s18_09_type_any() {
    ok_syntax("VALUE x :: ANY = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_any_value() {
    ok_syntax("VALUE x :: ANY VALUE = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_any_property_value() {
    ok_syntax("VALUE x :: ANY PROPERTY VALUE = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_closed_union_pipe() {
    ok_syntax("VALUE x :: INT32 | STRING = 1 MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_closed_union_any_angle() {
    ok_syntax("VALUE x :: ANY<INT32 | STRING> = 1 MATCH (n) RETURN n");
}

// §18.9 — Value types: Immaterial types
#[test]
fn s18_09_type_null() {
    ok_syntax("VALUE x :: NULL = NULL MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_nothing() {
    ok_syntax("VALUE x :: NOTHING = NULL MATCH (n) RETURN n");
}

#[test]
fn s18_09_type_not_null() {
    ok_syntax("VALUE x :: INT32 NOT NULL = 1 MATCH (n) RETURN n");
}

// §18.10 — Field type (typed keyword in record fields)
#[test]
fn s18_10_field_type_typed_keyword() {
    ok_syntax(
        "VALUE x :: RECORD {name TYPED STRING, age :: INT32} = {name: 'A', age: 1} MATCH (n) RETURN n",
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// §19 — Predicates
// ════════════════════════════════════════════════════════════════════════════════

// §19.3 — Comparison predicate
#[test]
fn s19_03_comparison_eq() {
    ok("MATCH (n) WHERE n.x = 1 RETURN n");
}

#[test]
fn s19_03_comparison_ne() {
    ok("MATCH (n) WHERE n.x <> 1 RETURN n");
}

#[test]
fn s19_03_comparison_lt() {
    ok("MATCH (n) WHERE n.x < 1 RETURN n");
}

#[test]
fn s19_03_comparison_gt() {
    ok("MATCH (n) WHERE n.x > 1 RETURN n");
}

#[test]
fn s19_03_comparison_le() {
    ok("MATCH (n) WHERE n.x <= 1 RETURN n");
}

#[test]
fn s19_03_comparison_ge() {
    ok("MATCH (n) WHERE n.x >= 1 RETURN n");
}

// §19.4 — EXISTS predicate
#[test]
fn s19_04_exists_pattern() {
    ok("MATCH (n) WHERE EXISTS { (n)-[:KNOWS]->() } RETURN n");
}

#[test]
fn s19_04_exists_subquery_braces() {
    ok("MATCH (n) WHERE EXISTS { MATCH (n)-[:KNOWS]->(m) RETURN m } RETURN n");
}

#[test]
fn s19_04_exists_pattern_parens() {
    ok("MATCH (n) WHERE EXISTS ((n)-[:KNOWS]->()) RETURN n");
}

// §19.5 — NULL predicate
#[test]
fn s19_05_is_null() {
    ok("MATCH (n) WHERE n.x IS NULL RETURN n");
}

#[test]
fn s19_05_is_not_null() {
    ok("MATCH (n) WHERE n.x IS NOT NULL RETURN n");
}

// §19.6 — Value type predicate
#[test]
fn s19_06_is_typed() {
    ok("MATCH (n) WHERE n.x IS TYPED INT32 RETURN n");
}

#[test]
fn s19_06_is_not_typed() {
    ok("MATCH (n) WHERE n.x IS NOT TYPED STRING RETURN n");
}

// §19.7 — Normalized predicate
#[test]
fn s19_07_is_normalized() {
    ok("MATCH (n) WHERE n.name IS NORMALIZED RETURN n");
}

#[test]
fn s19_07_is_nfc_normalized() {
    ok("MATCH (n) WHERE n.name IS NFC NORMALIZED RETURN n");
}

#[test]
fn s19_07_is_nfkd_normalized() {
    ok("MATCH (n) WHERE n.name IS NFKD NORMALIZED RETURN n");
}

#[test]
fn s19_07_is_not_normalized() {
    ok("MATCH (n) WHERE n.name IS NOT NORMALIZED RETURN n");
}

// §19.8 — Directed predicate
#[test]
fn s19_08_is_directed() {
    ok_syntax("MATCH (a)-[e]->(b) WHERE e IS DIRECTED RETURN e");
}

#[test]
fn s19_08_is_not_directed() {
    ok_syntax("MATCH (a)-[e]->(b) WHERE e IS NOT DIRECTED RETURN e");
}

// §19.9 — Labeled predicate
#[test]
fn s19_09_is_labeled() {
    ok("MATCH (n) WHERE n IS LABELED Person RETURN n");
}

#[test]
fn s19_09_is_not_labeled() {
    ok("MATCH (n) WHERE n IS NOT LABELED Person RETURN n");
}

#[test]
fn s19_09_colon_label() {
    ok("MATCH (n) WHERE n :Person RETURN n");
}

// §19.10 — Source/destination predicate
#[test]
fn s19_10_is_source_of() {
    ok_syntax("MATCH (a)-[e]->(b) WHERE a IS SOURCE OF e RETURN a");
}

#[test]
fn s19_10_is_not_source_of() {
    ok_syntax("MATCH (a)-[e]->(b) WHERE a IS NOT SOURCE OF e RETURN a");
}

#[test]
fn s19_10_is_destination_of() {
    ok_syntax("MATCH (a)-[e]->(b) WHERE b IS DESTINATION OF e RETURN b");
}

// §19.11 — ALL_DIFFERENT predicate
#[test]
fn s19_11_all_different() {
    ok_syntax("MATCH (a)-[e]->(b) WHERE ALL_DIFFERENT(a, b) RETURN a");
}

// §19.12 — SAME predicate
#[test]
fn s19_12_same() {
    ok_syntax("MATCH (a)-[e]->(b) WHERE SAME(a, b) RETURN a");
}

// §19.13 — PROPERTY_EXISTS predicate
#[test]
fn s19_13_property_exists() {
    ok("MATCH (n) WHERE PROPERTY_EXISTS(n, name) RETURN n");
}

// ════════════════════════════════════════════════════════════════════════════════
// §20 — Value expressions & functions
// ════════════════════════════════════════════════════════════════════════════════

// §20.1 — Arithmetic
#[test]
fn s20_01_arithmetic_add() {
    ok("MATCH (n) RETURN n.x + n.y");
}

#[test]
fn s20_01_arithmetic_sub() {
    ok("MATCH (n) RETURN n.x - n.y");
}

#[test]
fn s20_01_arithmetic_mul() {
    ok("MATCH (n) RETURN n.x * n.y");
}

#[test]
fn s20_01_arithmetic_div() {
    ok("MATCH (n) RETURN n.x / n.y");
}

#[test]
fn s20_01_unary_neg() {
    ok("MATCH (n) RETURN -n.x");
}

#[test]
fn s20_01_unary_pos() {
    ok("MATCH (n) RETURN +n.x");
}

// §20.2 — Value expression primary
#[test]
fn s20_02_parenthesized() {
    ok("MATCH (n) RETURN (n.x + 1) * 2");
}

// §20.3 — Value specification (literals)
#[test]
fn s20_03_integer_literal() {
    ok("MATCH (n) WHERE n.x = 42 RETURN n");
}

#[test]
fn s20_03_float_literal() {
    ok("MATCH (n) WHERE n.x = 3.14 RETURN n");
}

#[test]
fn s20_03_string_literal() {
    ok("MATCH (n) WHERE n.name = 'hello' RETURN n");
}

#[test]
fn s20_03_boolean_true() {
    ok("MATCH (n) WHERE n.active = TRUE RETURN n");
}

#[test]
fn s20_03_boolean_false() {
    ok("MATCH (n) WHERE n.active = FALSE RETURN n");
}

#[test]
fn s20_03_null_literal() {
    ok("MATCH (n) WHERE n.x = NULL RETURN n");
}

#[test]
fn s20_03_bytes_literal() {
    ok_syntax("MATCH (n) RETURN X'DEADBEEF'");
}

#[test]
fn s20_03_decimal_literal() {
    ok_syntax("MATCH (n) RETURN 123.45M");
}

// §20.4 — Dynamic parameter specification
#[test]
fn s20_04_parameter() {
    ok("MATCH (n) WHERE n.x = $param RETURN n");
}

// §20.5 — LET value expression
#[test]
fn s20_05_let_expr() {
    ok("MATCH (n) RETURN LET x = n.age IN x * 2 END");
}

#[test]
fn s20_05_let_expr_multiple_bindings() {
    ok("MATCH (n) RETURN LET x = n.age, y = n.score IN x + y END");
}

// §20.6 — Value query expression
#[test]
fn s20_06_value_subquery() {
    ok_syntax("MATCH (n) RETURN VALUE { MATCH (m) WHERE m.id = n.id RETURN m.score }");
}

// §20.7 — CASE expression
#[test]
fn s20_07_case_simple() {
    ok(
        "MATCH (n) RETURN CASE n.status WHEN 'A' THEN 'Active' WHEN 'I' THEN 'Inactive' ELSE 'Unknown' END",
    );
}

#[test]
fn s20_07_case_searched() {
    ok("MATCH (n) RETURN CASE WHEN n.age > 18 THEN 'Adult' ELSE 'Minor' END");
}

#[test]
fn s20_07_coalesce() {
    ok("MATCH (n) RETURN COALESCE(n.name, 'unknown')");
}

#[test]
fn s20_07_nullif() {
    ok("MATCH (n) RETURN NULLIF(n.x, 0)");
}

// §20.8 — CAST specification
#[test]
fn s20_08_cast() {
    ok("MATCH (n) RETURN CAST(n.x AS STRING)");
}

#[test]
fn s20_08_cast_complex_type() {
    ok("MATCH (n) RETURN CAST(n.x AS LIST<INT32>)");
}

// §20.9 — Aggregate functions
#[test]
fn s20_09_count_star() {
    ok("MATCH (n) RETURN COUNT(*)");
}

#[test]
fn s20_09_count() {
    ok("MATCH (n) RETURN COUNT(n)");
}

#[test]
fn s20_09_count_distinct() {
    ok("MATCH (n) RETURN COUNT(DISTINCT n.label)");
}

#[test]
fn s20_09_sum() {
    ok("MATCH (n) RETURN SUM(n.x)");
}

#[test]
fn s20_09_avg() {
    ok("MATCH (n) RETURN AVG(n.x)");
}

#[test]
fn s20_09_min() {
    ok("MATCH (n) RETURN MIN(n.x)");
}

#[test]
fn s20_09_max() {
    ok("MATCH (n) RETURN MAX(n.x)");
}

#[test]
fn s20_09_collect_list() {
    ok("MATCH (n) RETURN COLLECT_LIST(n.x)");
}

#[test]
fn s20_09_stddev_samp() {
    ok("MATCH (n) RETURN STDDEV_SAMP(n.x)");
}

#[test]
fn s20_09_stddev_pop() {
    ok("MATCH (n) RETURN STDDEV_POP(n.x)");
}

#[test]
fn s20_09_percentile_cont() {
    ok_syntax("MATCH (n) RETURN PERCENTILE_CONT(n.x, 0.5)");
}

#[test]
fn s20_09_percentile_disc() {
    ok_syntax("MATCH (n) RETURN PERCENTILE_DISC(n.x, 0.5)");
}

// §20.10 — ELEMENT_ID function
#[test]
fn s20_10_element_id() {
    ok("MATCH (n) RETURN ELEMENT_ID(n)");
}

// §20.11 — Property reference
#[test]
fn s20_11_property_access() {
    ok("MATCH (n) RETURN n.name");
}

#[test]
fn s20_11_nested_property_access() {
    ok("MATCH (n) RETURN n.address.city");
}

// §20.14 — Path value constructor
#[test]
fn s20_14_path_constructor() {
    ok_syntax("MATCH (a), (b) RETURN PATH[a, 1, b]");
}

// §20.15 — List value expression (concatenation)
#[test]
fn s20_15_list_concat() {
    ok_syntax("MATCH (n) RETURN [1, 2] || [3, 4]");
}

// §20.16 — List value function
#[test]
fn s20_16_elements() {
    ok_syntax("MATCH p = (a)-[]->(b) RETURN ELEMENTS(p)");
}

#[test]
fn s20_16_trim_list() {
    ok_syntax("MATCH (n) RETURN TRIM([1, 2, 3], 1)");
}

// §20.17 — List value constructor
#[test]
fn s20_17_list_literal() {
    ok_syntax("MATCH (n) RETURN [1, 2, 3]");
}

#[test]
fn s20_17_list_constructor_keyword() {
    ok_syntax("MATCH (n) RETURN LIST[1, 2, 3]");
}

#[test]
fn s20_17_array_constructor_keyword() {
    ok_syntax("MATCH (n) RETURN ARRAY[1, 2, 3]");
}

// §20.18 — Record constructor
#[test]
fn s20_18_record_literal() {
    ok_syntax("MATCH (n) RETURN {name: 'Alice', age: 30}");
}

#[test]
fn s20_18_record_constructor_keyword() {
    ok_syntax("MATCH (n) RETURN RECORD {name: 'Alice', age: 30}");
}

// §20.20 — Boolean value expression
#[test]
fn s20_20_and() {
    ok("MATCH (n) WHERE n.x > 1 AND n.y < 10 RETURN n");
}

#[test]
fn s20_20_or() {
    ok("MATCH (n) WHERE n.x > 1 OR n.y < 10 RETURN n");
}

#[test]
fn s20_20_not() {
    ok("MATCH (n) WHERE NOT n.active RETURN n");
}

#[test]
fn s20_20_xor() {
    ok("MATCH (n) WHERE n.a XOR n.b RETURN n");
}

// §20.20 — IS TRUE / FALSE / UNKNOWN
#[test]
fn s20_20_is_true() {
    ok("MATCH (n) WHERE n.active IS TRUE RETURN n");
}

#[test]
fn s20_20_is_false() {
    ok("MATCH (n) WHERE n.active IS FALSE RETURN n");
}

#[test]
fn s20_20_is_unknown() {
    ok("MATCH (n) WHERE n.active IS UNKNOWN RETURN n");
}

#[test]
fn s20_20_is_not_true() {
    ok("MATCH (n) WHERE n.active IS NOT TRUE RETURN n");
}

// §20.21 — String concatenation
#[test]
fn s20_21_string_concat() {
    ok("MATCH (n) RETURN n.first || ' ' || n.last");
}

// §20.22 — Numeric value functions
#[test]
fn s20_22_abs() {
    ok("MATCH (n) RETURN ABS(n.x)");
}

#[test]
fn s20_22_mod() {
    ok("MATCH (n) RETURN MOD(n.x, 3)");
}

#[test]
fn s20_22_floor() {
    ok("MATCH (n) RETURN FLOOR(n.x)");
}

#[test]
fn s20_22_ceil() {
    ok("MATCH (n) RETURN CEIL(n.x)");
}

#[test]
fn s20_22_ceiling() {
    ok_syntax("MATCH (n) RETURN CEILING(n.x)");
}

#[test]
fn s20_22_sqrt() {
    ok("MATCH (n) RETURN SQRT(n.x)");
}

#[test]
fn s20_22_exp() {
    ok("MATCH (n) RETURN EXP(n.x)");
}

#[test]
fn s20_22_power() {
    ok("MATCH (n) RETURN POWER(n.x, 2)");
}

#[test]
fn s20_22_ln() {
    ok("MATCH (n) RETURN LN(n.x)");
}

#[test]
fn s20_22_log() {
    ok("MATCH (n) RETURN LOG(10, n.x)");
}

#[test]
fn s20_22_log10() {
    ok("MATCH (n) RETURN LOG10(n.x)");
}

#[test]
fn s20_22_sin() {
    ok("MATCH (n) RETURN SIN(n.x)");
}

#[test]
fn s20_22_cos() {
    ok("MATCH (n) RETURN COS(n.x)");
}

#[test]
fn s20_22_tan() {
    ok("MATCH (n) RETURN TAN(n.x)");
}

#[test]
fn s20_22_cot() {
    ok_syntax("MATCH (n) RETURN COT(n.x)");
}

#[test]
fn s20_22_sinh() {
    ok_syntax("MATCH (n) RETURN SINH(n.x)");
}

#[test]
fn s20_22_cosh() {
    ok_syntax("MATCH (n) RETURN COSH(n.x)");
}

#[test]
fn s20_22_tanh() {
    ok_syntax("MATCH (n) RETURN TANH(n.x)");
}

#[test]
fn s20_22_asin() {
    ok("MATCH (n) RETURN ASIN(n.x)");
}

#[test]
fn s20_22_acos() {
    ok("MATCH (n) RETURN ACOS(n.x)");
}

#[test]
fn s20_22_atan() {
    ok("MATCH (n) RETURN ATAN(n.x)");
}

#[test]
fn s20_22_degrees() {
    ok("MATCH (n) RETURN DEGREES(n.x)");
}

#[test]
fn s20_22_radians() {
    ok("MATCH (n) RETURN RADIANS(n.x)");
}

// §20.22 — Length functions
#[test]
fn s20_22_char_length() {
    ok("MATCH (n) RETURN CHAR_LENGTH(n.name)");
}

#[test]
fn s20_22_character_length() {
    ok("MATCH (n) RETURN CHARACTER_LENGTH(n.name)");
}

#[test]
fn s20_22_byte_length() {
    ok("MATCH (n) RETURN BYTE_LENGTH(n.data)");
}

#[test]
fn s20_22_octet_length() {
    ok("MATCH (n) RETURN OCTET_LENGTH(n.data)");
}

#[test]
fn s20_22_path_length() {
    ok_syntax("MATCH p = (a)-[]->(b) RETURN PATH_LENGTH(p)");
}

#[test]
fn s20_22_cardinality() {
    ok_syntax("MATCH (n) RETURN CARDINALITY(n.items)");
}

#[test]
fn s20_22_size() {
    ok_syntax("MATCH (n) RETURN SIZE(n.items)");
}

// §20.24 — String value functions
#[test]
fn s20_24_upper() {
    ok("MATCH (n) RETURN UPPER(n.name)");
}

#[test]
fn s20_24_lower() {
    ok("MATCH (n) RETURN LOWER(n.name)");
}

#[test]
fn s20_24_trim() {
    ok("MATCH (n) RETURN TRIM(n.name)");
}

#[test]
fn s20_24_trim_leading() {
    ok("MATCH (n) RETURN TRIM(LEADING FROM n.name)");
}

#[test]
fn s20_24_trim_trailing() {
    ok("MATCH (n) RETURN TRIM(TRAILING FROM n.name)");
}

#[test]
fn s20_24_trim_both() {
    ok("MATCH (n) RETURN TRIM(BOTH FROM n.name)");
}

#[test]
fn s20_24_trim_char() {
    ok("MATCH (n) RETURN TRIM(BOTH 'x' FROM n.name)");
}

#[test]
fn s20_24_btrim() {
    ok("MATCH (n) RETURN BTRIM(n.name)");
}

#[test]
fn s20_24_ltrim() {
    ok("MATCH (n) RETURN LTRIM(n.name)");
}

#[test]
fn s20_24_rtrim() {
    ok("MATCH (n) RETURN RTRIM(n.name)");
}

#[test]
fn s20_24_btrim_with_chars() {
    ok("MATCH (n) RETURN BTRIM(n.name, 'xyz')");
}

#[test]
fn s20_24_left_function() {
    ok("MATCH (n) RETURN LEFT(n.name, 5)");
}

#[test]
fn s20_24_right_function() {
    ok("MATCH (n) RETURN RIGHT(n.name, 5)");
}

#[test]
fn s20_24_normalize() {
    ok("MATCH (n) RETURN NORMALIZE(n.name)");
}

#[test]
fn s20_24_normalize_nfkc() {
    ok("MATCH (n) RETURN NORMALIZE(n.name, NFKC)");
}

// §20.25 — Session user
#[test]
fn s20_25_session_user() {
    ok_syntax("MATCH (n) RETURN SESSION_USER");
}

// §20.27 — Datetime value functions
#[test]
fn s20_27_current_date() {
    ok_syntax("MATCH (n) RETURN CURRENT_DATE");
}

#[test]
fn s20_27_current_time() {
    ok_syntax("MATCH (n) RETURN CURRENT_TIME");
}

#[test]
fn s20_27_current_timestamp() {
    ok_syntax("MATCH (n) RETURN CURRENT_TIMESTAMP");
}

#[test]
fn s20_27_local_time_bare() {
    ok_syntax("MATCH (n) RETURN LOCAL_TIME");
}

#[test]
fn s20_27_local_timestamp_bare() {
    ok_syntax("MATCH (n) RETURN LOCAL_TIMESTAMP");
}

// §20.27 — Datetime constructors
#[test]
fn s20_27_date_literal() {
    ok_syntax("MATCH (n) RETURN DATE '2024-01-15'");
}

#[test]
fn s20_27_time_literal() {
    ok_syntax("MATCH (n) RETURN TIME '12:30:00'");
}

#[test]
fn s20_27_datetime_literal() {
    ok_syntax("MATCH (n) RETURN DATETIME '2024-01-15T12:30:00'");
}

#[test]
fn s20_27_timestamp_literal() {
    ok_syntax("MATCH (n) RETURN TIMESTAMP '2024-01-15T12:30:00'");
}

#[test]
fn s20_27_duration_literal() {
    ok_syntax("MATCH (n) RETURN DURATION 'P1Y2M'");
}

#[test]
fn s20_27_date_function() {
    ok_syntax("MATCH (n) RETURN DATE(2024, 1, 15)");
}

#[test]
fn s20_27_local_datetime_function() {
    ok_syntax("MATCH (n) RETURN LOCAL_DATETIME(2024, 1, 15, 12, 0, 0)");
}

#[test]
fn s20_27_local_time_function() {
    ok_syntax("MATCH (n) RETURN LOCAL_TIME(12, 30, 0)");
}

#[test]
fn s20_27_local_timestamp_is_bare_only() {
    // GQL: LOCAL_TIMESTAMP is always bare (no parens form).
    // LOCAL_DATETIME(...) is the parens variant.
    ok_syntax("MATCH (n) RETURN LOCAL_TIMESTAMP");
}

#[test]
fn s20_27_zoned_time_function() {
    ok_syntax("MATCH (n) RETURN ZONED_TIME(12, 30, 0, 'UTC')");
}

#[test]
fn s20_27_zoned_datetime_function() {
    ok_syntax("MATCH (n) RETURN ZONED_DATETIME(2024, 1, 15, 12, 0, 0, 'UTC')");
}

#[test]
fn s20_27_duration_function() {
    ok_syntax("MATCH (n) RETURN DURATION(1, 2, 3, 4)");
}

// §20.29 — Duration value function
#[test]
fn s20_29_duration_between() {
    ok_syntax("MATCH (n) RETURN DURATION_BETWEEN(n.start, n.finish)");
}

#[test]
fn s20_29_duration_between_year_to_month() {
    ok_syntax("MATCH (n) RETURN DURATION_BETWEEN(n.start, n.finish) YEAR TO MONTH");
}

#[test]
fn s20_29_duration_between_day_to_second() {
    ok_syntax("MATCH (n) RETURN DURATION_BETWEEN(n.start, n.finish) DAY TO SECOND");
}

// ════════════════════════════════════════════════════════════════════════════════
// §21 — Literals and tokens
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn s21_02_unsigned_integer() {
    ok("MATCH (n) WHERE n.x = 0 RETURN n");
}

#[test]
fn s21_02_negative_integer() {
    ok("MATCH (n) WHERE n.x = -42 RETURN n");
}

#[test]
fn s21_02_scientific_float() {
    ok_syntax("MATCH (n) WHERE n.x = 1.5e10 RETURN n");
}

#[test]
fn s21_03_quoted_identifier() {
    ok_syntax("MATCH (`node with spaces`) RETURN `node with spaces`");
}

// ════════════════════════════════════════════════════════════════════════════════
// §18.1–18.3 — Graph type elements (node/edge type patterns)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn s18_02_phrase_node_type() {
    ok_syntax(
        "CREATE GRAPH TYPE myType { NODE Person LABEL Person { name STRING NOT NULL, age INT32 } }",
    );
    ok_syntax(
        "CREATE GRAPH TYPE myType { NODE Person LABEL Person { name STRING NOT NULL, age INT32, identity PRINCIPAL } }",
    );
}

#[test]
fn s18_03_phrase_edge_type() {
    ok_syntax(
        "CREATE GRAPH TYPE myType { NODE A LABEL A, NODE B LABEL B, DIRECTED EDGE R LABEL R CONNECTING (A -> B) }",
    );
}

#[test]
fn s18_03_undirected_edge_type() {
    ok_syntax(
        "CREATE GRAPH TYPE myType { NODE A LABEL A, NODE B LABEL B, UNDIRECTED EDGE R LABEL R CONNECTING (A ~ B) }",
    );
}

#[test]
fn s18_pattern_node_type() {
    ok_syntax("CREATE GRAPH myGraph { (:Person {name STRING}) }");
}

#[test]
fn s18_pattern_edge_type_right() {
    ok_syntax("CREATE GRAPH myGraph { (:A)-[:R]->(:B) }");
}

#[test]
fn s18_pattern_edge_type_left() {
    ok_syntax("CREATE GRAPH myGraph { (:B)<-[:R]-(:A) }");
}

#[test]
fn s18_pattern_edge_type_undirected() {
    ok_syntax("CREATE GRAPH myGraph { (:A)~[:R]~(:B) }");
}

// ════════════════════════════════════════════════════════════════════════════════
// NEXT chaining (§6 statementBlock)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn s06_next_chain() {
    ok_syntax("MATCH (n) RETURN n NEXT MATCH (m) RETURN m");
}

#[test]
fn s06_next_chain_yield() {
    ok_syntax("MATCH (n) RETURN n NEXT YIELD n MATCH (n)-[e]->(m) RETURN m");
}

// ════════════════════════════════════════════════════════════════════════════════
// Inline element properties (WHERE in node/edge pattern)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn element_pattern_properties() {
    ok("MATCH (n:Person {name: 'Alice'}) RETURN n");
}

#[test]
fn element_pattern_where() {
    ok("MATCH (n:Person WHERE n.age > 30) RETURN n");
}

#[test]
fn edge_pattern_properties() {
    ok("MATCH (a)-[e:KNOWS {since: 2020}]->(b) RETURN a, b");
}

// ════════════════════════════════════════════════════════════════════════════════
// Generic function call fallback
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn generic_function_call() {
    ok_syntax("MATCH (n) RETURN myFunc(n.x, n.y)");
}

#[test]
fn generic_function_call_no_args() {
    ok_syntax("MATCH (n) RETURN myFunc()");
}

#[test]
fn generic_function_call_distinct() {
    ok_syntax("MATCH (n) RETURN myFunc(DISTINCT n.x)");
}
