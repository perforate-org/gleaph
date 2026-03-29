use gleaph_gql::ast::{CreateStmt, Expr, Statement};
use gleaph_gql::{parse_statement, validate_statement};
use gleaph_types::GleaphError;

// ── SET / REMOVE ──────────────────────────────────────────────────────────────

#[test]
fn set_property_parses_and_validates() {
    let cases = [
        r#"MATCH (a:User)-[:KNOWS]->(b) WHERE a.name = 'Alice' SET a.score = 42"#,
        r#"MATCH (a:User)-[:KNOWS]->(b) SET a.score = b.score + 1"#,
        r#"MATCH (a:User)-[e:KNOWS]->(b) WHERE a.name = 'X' SET e.weight = 0.5"#,
    ];
    for gql in cases {
        let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse {gql}: {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate {gql}: {e}"));
    }
}

#[test]
fn set_label_parses_and_validates() {
    let gql = r#"MATCH (a:User)-[:KNOWS]->(b) WHERE a.name = 'Alice' SET a:Admin"#;
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn set_all_properties_parses() {
    use gleaph_gql::ast::{SetItem, Statement};
    let gql = r#"MATCH (a:User)-[:KNOWS]->(b) SET a = {name: 'Bob', age: 30}"#;
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
    if let Statement::Set(s) = &stmt {
        assert_eq!(s.set_clause.items.len(), 1);
        match &s.set_clause.items[0] {
            SetItem::AllProperties { var, properties } => {
                assert_eq!(var, "a");
                assert_eq!(properties.len(), 2);
                assert_eq!(properties[0].0, "name");
                assert_eq!(properties[1].0, "age");
            }
            other => panic!("expected AllProperties, got {other:?}"),
        }
    } else {
        panic!("expected Set statement");
    }
}

#[test]
fn set_all_properties_empty_map_parses() {
    let gql = r#"MATCH (a:User)-[:KNOWS]->(b) SET a = {}"#;
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn remove_property_parses_and_validates() {
    let cases = [
        r#"MATCH (a:User)-[:KNOWS]->(b) WHERE a.name = 'Alice' REMOVE a.score"#,
        r#"MATCH (a:User)-[e:KNOWS]->(b) REMOVE e.weight"#,
    ];
    for gql in cases {
        let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse {gql}: {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate {gql}: {e}"));
    }
}

#[test]
fn remove_label_parses_and_validates() {
    let gql = r#"MATCH (a:User)-[:KNOWS]->(b) WHERE a.name = 'Alice' REMOVE a:Admin"#;
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn detach_delete_parses_and_validates() {
    let gql = r#"MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.name = 'Bob' DETACH DELETE b"#;
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
}

// ── WITH / DISTINCT / OFFSET ──────────────────────────────────────────────────

#[test]
fn with_clause_parses_and_validates() {
    let cases = [
        "MATCH (a:User)-[:KNOWS]->(b:User) WITH b.name AS name RETURN name",
        "MATCH (a:User)-[:KNOWS]->(b:User) WITH a, b.name AS bname RETURN a, bname",
        "MATCH (a:User)-[:KNOWS]->(b:User) WITH COUNT(*) AS cnt RETURN cnt",
    ];
    for gql in cases {
        let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse {gql}: {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate {gql}: {e}"));
    }
}

#[test]
fn distinct_return_parses_and_validates() {
    let stmt = parse_statement("MATCH (a:User)-[:KNOWS]->(b:User) RETURN DISTINCT b.name").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn offset_parses_and_validates() {
    let stmt = parse_statement("MATCH (a:User)-[:KNOWS]->(b:User) RETURN b.name LIMIT 10 OFFSET 5")
        .unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn skip_keyword_rejected() {
    // SKIP is a Cypher-ism; GQL standard uses OFFSET only
    let err = parse_statement("MATCH (a:User)-[:KNOWS]->(b:User) RETURN b.name SKIP 3");
    assert!(err.is_err());
}

#[test]
fn offset_keyword_accepted() {
    let stmt = parse_statement("MATCH (a:User)-[:KNOWS]->(b:User) RETURN b.name OFFSET 3").unwrap();
    validate_statement(&stmt).unwrap();
}

// ── UNION / EXCEPT ────────────────────────────────────────────────────────────

#[test]
fn union_parses_and_validates() {
    let gql = "MATCH (a:User)-[:KNOWS]->(b:User) RETURN a.name UNION MATCH (a:User)-[:LIKES]->(b:User) RETURN a.name";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn union_all_parses_and_validates() {
    let gql = "MATCH (a:User)-[:KNOWS]->(b:User) RETURN a.name UNION ALL MATCH (a:User)-[:LIKES]->(b:User) RETURN a.name";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn except_parses_and_validates() {
    let gql = "MATCH (a:User)-[:KNOWS]->(b:User) RETURN a.name EXCEPT MATCH (a:User)-[:LIKES]->(b:User) RETURN a.name";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
}

// ── GROUP BY / HAVING ─────────────────────────────────────────────────────────

#[test]
fn group_by_parses_and_validates() {
    let gql = "MATCH (a:User)-[:KNOWS]->(b:User) RETURN a.name, COUNT(*) GROUP BY a.name";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn having_parses_and_validates() {
    let gql = "MATCH (a:User)-[:KNOWS]->(b:User) RETURN a.name, COUNT(*) GROUP BY a.name HAVING COUNT(*) > 1";
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
}

// ── AGGREGATION FUNCTIONS ─────────────────────────────────────────────────────

#[test]
fn all_aggregation_functions_parse_and_validate() {
    let cases = [
        "MATCH (a:User)-[:KNOWS]->(b:User) RETURN COUNT(*)",
        "MATCH (a:User)-[:KNOWS]->(b:User) RETURN COUNT(b.score)",
        "MATCH (a:User)-[:KNOWS]->(b:User) RETURN COUNT(DISTINCT b.name)",
        "MATCH (a:User)-[:KNOWS]->(b:User) RETURN SUM(b.score)",
        "MATCH (a:User)-[:KNOWS]->(b:User) RETURN AVG(b.score)",
        "MATCH (a:User)-[:KNOWS]->(b:User) RETURN MIN(b.score)",
        "MATCH (a:User)-[:KNOWS]->(b:User) RETURN MAX(b.score)",
        "MATCH (a:User)-[:KNOWS]->(b:User) RETURN COLLECT(b.name)",
    ];
    for gql in cases {
        let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse {gql}: {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate {gql}: {e}"));
    }
}

// ── EXISTS ────────────────────────────────────────────────────────────────────

#[test]
fn exists_subquery_parses_and_validates() {
    let gql = r#"MATCH (a:User)-[:KNOWS]->(b:User) WHERE EXISTS { MATCH (b)-[:LIKES]->(c) RETURN c } RETURN a.name"#;
    let stmt = parse_statement(gql).unwrap();
    validate_statement(&stmt).unwrap();
}

// ── NULL PREDICATES / IN ──────────────────────────────────────────────────────

#[test]
fn is_null_and_is_not_null_parse_and_validate() {
    let cases = [
        "MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.score IS NULL RETURN a.name",
        "MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.score IS NOT NULL RETURN a.name",
        "MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.score IS NULL OR b.name IS NOT NULL RETURN b.name",
    ];
    for gql in cases {
        let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse {gql}: {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate {gql}: {e}"));
    }
}

#[test]
fn in_list_predicate_parses_and_validates() {
    let cases = [
        r#"MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.name IN ['Alice', 'Bob'] RETURN b.name"#,
        "MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.score IN [1, 2, 3] RETURN b.name",
    ];
    for gql in cases {
        let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse {gql}: {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate {gql}: {e}"));
    }
}

// ── VARIABLE-LENGTH PATH / SHORTEST (complement to existing gql_parser.rs) ───

#[test]
fn shortest_path_with_path_variable_parses_and_validates() {
    let cases = [
        "MATCH SHORTEST p = (a:User)-[:STEP*1..3]->(b:User) RETURN p",
        "MATCH SHORTEST (a:User)-[:STEP*1..3]->(b:User) RETURN a.name, b.name",
    ];
    for gql in cases {
        let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse {gql}: {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate {gql}: {e}"));
    }
}

#[test]
fn variable_length_path_edge_cases_parse_and_validate() {
    let cases = [
        // single-hop range
        "MATCH (a:User)-[:STEP*1..1]->(b:User) RETURN a.name, b.name",
        // max range allowed (10)
        "MATCH (a:User)-[:STEP*1..10]->(b:User) RETURN a.name, b.name",
        // no-label range
        "MATCH (a:User)-[*2..3]->(b:User) RETURN a.name, b.name",
    ];
    for gql in cases {
        let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse {gql}: {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate {gql}: {e}"));
    }
}

// ── NEGATIVE CASES ────────────────────────────────────────────────────────────

#[test]
fn case_expression_is_supported() {
    // CASE expressions are now fully supported (§20.4 CASE implemented).
    let gql =
        r#"MATCH (a:User)-[:KNOWS]->(b) RETURN CASE a.score WHEN 1 THEN 'low' ELSE 'high' END"#;
    let stmt = parse_statement(gql).expect("CASE should be accepted");
    let Statement::Query(q) = stmt else {
        panic!("expected Query, got {stmt:?}");
    };
    assert_eq!(q.return_clause.items.len(), 1);
    assert!(matches!(q.return_clause.items[0].expr, Expr::Case(_)));
}

#[test]
fn set_without_edge_hop_is_rejected() {
    // SET match clauses require 1-3 hops per validate_feature_gates.
    let e = parse_statement(r#"MATCH (a:User) SET a.score = 1"#)
        .and_then(|s| validate_statement(&s).map(|()| s))
        .expect_err("0-hop SET should be rejected");
    assert!(matches!(e, GleaphError::UnsupportedFeature(_)));
}

#[test]
fn remove_without_edge_hop_is_rejected() {
    let e = parse_statement(r#"MATCH (a:User) REMOVE a.score"#)
        .and_then(|s| validate_statement(&s).map(|()| s))
        .expect_err("0-hop REMOVE should be rejected");
    assert!(matches!(e, GleaphError::UnsupportedFeature(_)));
}

#[test]
fn variable_length_path_out_of_range_is_rejected() {
    // max > 10
    let e = parse_statement("MATCH (a:User)-[:STEP*1..11]->(b) RETURN a")
        .and_then(|s| validate_statement(&s).map(|()| s))
        .expect_err("max=11 should be rejected");
    assert!(matches!(e, GleaphError::ValidationError(_)));
}

// ── WITH … MATCH continuation ─────────────────────────────────────────────────

#[test]
fn with_match_continuation_parses_and_validates() {
    let cases = [
        // Basic continuation: variables from WITH used as anchors in next MATCH.
        "MATCH (a:User)-[:KNOWS]->(b:User) WITH a, b MATCH (b)-[:LIKES]->(c) RETURN c",
        // Continuation with WHERE after the follow-on MATCH.
        "MATCH (a:User)-[:KNOWS]->(b:User) WITH a, b MATCH (b)-[:LIKES]->(c) WHERE c.name = 'Alice' RETURN c",
        // Chained: two consecutive WITH … MATCH stages.
        "MATCH (a)-[:X]->(b) WITH a, b MATCH (b)-[:Y]->(c) WITH b, c MATCH (c)-[:Z]->(d) RETURN d",
        // Projected alias carried through the continuation MATCH.
        "MATCH (a:User)-[:KNOWS]->(b:User) WITH a.name AS nm, b MATCH (b)-[:LIKES]->(c) RETURN nm, c",
        // OPTIONAL MATCH as continuation.
        "MATCH (a)-[:X]->(b) WITH a, b OPTIONAL MATCH (b)-[:Y]->(c) RETURN a, c",
    ];
    for gql in cases {
        let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    }
}

#[test]
fn with_match_continuation_undefined_var_rejected() {
    // `z` is not in scope after WITH projects only a and b.
    let e = parse_statement("MATCH (a)-[:X]->(b) WITH a, b MATCH (b)-[:Y]->(c) RETURN z")
        .and_then(|s| validate_statement(&s).map(|()| s))
        .expect_err("z not in scope");
    assert!(matches!(e, GleaphError::ValidationError(_)));
}

#[test]
fn with_match_continuation_post_where_undefined_var_rejected() {
    // `z` is not bound by the continuation MATCH.
    let e =
        parse_statement("MATCH (a)-[:X]->(b) WITH a, b MATCH (b)-[:Y]->(c) WHERE z = 1 RETURN c")
            .and_then(|s| validate_statement(&s).map(|()| s))
            .expect_err("z not in post-match where scope");
    assert!(matches!(e, GleaphError::ValidationError(_)));
}

// ── Tilde undirected edge syntax rejected ────────────────────────────────────

#[test]
fn tilde_undirected_edge_rejected() {
    // All tilde-based syntax is rejected — Gleaph is directed-only
    let cases = [
        "MATCH (a)~[e:KNOWS]~(b) RETURN a",  // ~[...]~
        "MATCH (a)~[e:KNOWS]~>(b) RETURN a", // ~[...]~>  (Tilde detected first)
        "MATCH (a)~/KNOWS/~(b) RETURN a",    // ~/L/~
        "MATCH (a)~/KNOWS/~>(b) RETURN a",   // ~/L/~>
    ];
    for gql in cases {
        let err = parse_statement(gql);
        assert!(err.is_err(), "expected error for tilde syntax: {gql}");
    }
}

#[test]
fn tilde_left_arrow_variants_rejected() {
    // <~ variants: lexer produces Lt + Tilde
    let cases = [
        "MATCH (a)<~[e:KNOWS]~(b) RETURN a", // <~[...]~
        "MATCH (a)<~/KNOWS/~(b) RETURN a",   // <~/L/~
    ];
    for gql in cases {
        let err = parse_statement(gql);
        assert!(err.is_err(), "expected error for <~ tilde syntax: {gql}");
    }
}

// ── Simplified edge syntax (GQL §16.12) ─────────────────────────────────────

#[test]
fn simplified_edge_syntax_parses() {
    use gleaph_gql::ast::{Direction, PathLength};
    // All 4 forms should parse successfully
    let cases = [
        ("MATCH (a)-/KNOWS/->(b) RETURN a", Direction::Outgoing),
        ("MATCH (a)<-/KNOWS/-(b) RETURN a", Direction::Incoming),
        ("MATCH (a)<-/KNOWS/->(b) RETURN a", Direction::Either),
        ("MATCH (a)-/KNOWS/-(b) RETURN a", Direction::Either),
    ];
    for (gql, expected_dir) in cases {
        let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
        let Statement::Query(q) = &stmt else {
            panic!("expected Query for {gql}")
        };
        let chain = &q.match_clauses[0].pattern.chain(0);
        assert_eq!(
            chain.edge.direction, expected_dir,
            "direction mismatch for {gql}"
        );
        assert_eq!(
            chain.edge.label,
            Some("KNOWS".into()),
            "label mismatch for {gql}"
        );
        assert_eq!(chain.edge.length, PathLength::Fixed(1));
        assert!(
            chain.edge.var.is_none(),
            "simplified edge should have no variable"
        );
    }
}

// ── Bidirectional bracket edge syntax -[e:L]- ───────────────────────────────

#[test]
fn bracket_either_direction_parses() {
    use gleaph_gql::ast::{Direction, PathLength};
    let stmt = parse_statement("MATCH (a)-[e:KNOWS]-(b) RETURN a")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate: {e}"));
    let Statement::Query(q) = &stmt else {
        panic!("expected Query")
    };
    let chain = &q.match_clauses[0].pattern.chain(0);
    assert_eq!(chain.edge.label, Some("KNOWS".into()));
    assert_eq!(chain.edge.direction, Direction::Either);
    assert_eq!(chain.edge.length, PathLength::Fixed(1));
    assert_eq!(chain.edge.var, Some("e".into()));
}

// ── ANY k PATHS (§16.6) ─────────────────────────────────────────────────────────

#[test]
fn any_k_paths_parses_and_stores_limit() {
    let stmt = parse_statement("MATCH ANY 3 PATHS (a:User)-[:KNOWS]->(b:User) RETURN a")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate: {e}"));
    let Statement::Query(q) = &stmt else {
        panic!("expected Query")
    };
    assert_eq!(q.match_clauses[0].any_paths, Some(3));
}

// ── Multiple INSERT / DELETE (§13) ──────────────────────────────

#[test]
fn insert_multiple_patterns_parses() {
    let stmt = parse_statement("INSERT (:A {x: 1}), (:B {y: 2}), (:C)").unwrap();
    let Statement::Create(ref cs) = stmt else {
        panic!("expected Create");
    };
    assert_eq!(cs.len(), 3);
    assert!(matches!(cs[0], CreateStmt::Node(_)));
    assert!(matches!(cs[1], CreateStmt::Node(_)));
    assert!(matches!(cs[2], CreateStmt::Node(_)));
}

#[test]
fn insert_mixed_node_and_edge_parses() {
    let stmt = parse_statement("INSERT (:Tag), (:A)-[:X]->(:B)").unwrap();
    let Statement::Create(ref cs) = stmt else {
        panic!("expected Create");
    };
    assert_eq!(cs.len(), 2);
    assert!(matches!(cs[0], CreateStmt::Node(_)));
    assert!(matches!(cs[1], CreateStmt::Edge(_)));
}

#[test]
fn delete_multiple_targets_parses() {
    let stmt = parse_statement("MATCH (a:X)-[:E]->(b:Y) DELETE a, b").unwrap();
    let Statement::Delete(ref d) = stmt else {
        panic!("expected Delete");
    };
    assert_eq!(d.target_vars, vec!["a", "b"]);
}

#[test]
fn nodetach_delete_parses() {
    let stmt = parse_statement("MATCH (a:X) NODETACH DELETE a").unwrap();
    let Statement::Delete(ref d) = stmt else {
        panic!("expected Delete");
    };
    assert!(d.nodetach);
    assert!(!d.detach);
    assert_eq!(d.target_vars, vec!["a"]);
}

// ── GRAPH TYPE (§12) ────────────────────────────────────────────

#[test]
fn create_graph_type_parses_with_body() {
    let stmt = parse_statement(
        "CREATE GRAPH TYPE SocialNet { (:Person), (:Company), -[:KNOWS]->, -[:WORKS_AT]-> }",
    )
    .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType {
            name,
            definition,
            if_not_exists,
            or_replace,
            source,
        } => {
            assert_eq!(name, "SocialNet");
            assert!(!if_not_exists);
            assert!(!or_replace);
            assert!(source.is_none());
            assert_eq!(definition.node_labels, vec!["Company", "Person"]);
            assert_eq!(definition.edge_labels, vec!["KNOWS", "WORKS_AT"]);
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

#[test]
fn drop_graph_type_parses_to_correct_statement() {
    let stmt =
        parse_statement("DROP GRAPH TYPE SocialNet").unwrap_or_else(|e| panic!("parse: {e}"));
    assert!(
        matches!(stmt, Statement::DropGraphType { ref name, if_exists: false } if name == "SocialNet")
    );
}

#[test]
fn graph_type_node_entry_without_colon() {
    let stmt = parse_statement("CREATE GRAPH TYPE T { (Person), (Company) }")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType { definition, .. } => {
            assert_eq!(definition.node_labels, vec!["Company", "Person"]);
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

#[test]
fn graph_type_undirected_edge_entry_rejected() {
    // Tilde syntax not supported in graph type definitions
    let err = parse_statement("CREATE GRAPH TYPE T { (:A), ~[:LINK]~ }");
    assert!(err.is_err(), "tilde edge in graph type should be rejected");
}

#[test]
fn graph_type_deduplicates_labels() {
    let stmt =
        parse_statement("CREATE GRAPH TYPE T { (:Person), (:Person), -[:KNOWS]->, -[:KNOWS]-> }")
            .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType { definition, .. } => {
            assert_eq!(definition.node_labels, vec!["Person"]);
            assert_eq!(definition.edge_labels, vec!["KNOWS"]);
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

#[test]
fn graph_type_empty_body_is_error() {
    let result = parse_statement("CREATE GRAPH TYPE T { }");
    assert!(result.is_err(), "expected error for empty body");
}

#[test]
fn graph_type_missing_body_is_error() {
    let result = parse_statement("CREATE GRAPH TYPE T");
    assert!(result.is_err(), "expected error for missing body");
}

#[test]
fn graph_type_edge_without_colon() {
    let stmt = parse_statement("CREATE GRAPH TYPE T { (:A), -[LINK]-> }")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType { definition, .. } => {
            assert_eq!(definition.edge_labels, vec!["LINK"]);
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

// ── GRAPH TYPE — inline edge types (§18.3) ─────────────────────

#[test]
fn graph_type_inline_edge_type_parses() {
    let stmt =
        parse_statement("CREATE GRAPH TYPE T { (:User), (:Post), (:User)-[:Posted]->(:Post) }")
            .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType { definition, .. } => {
            assert_eq!(definition.node_labels, vec!["Post", "User"]);
            assert!(definition.edge_labels.is_empty());
            assert_eq!(definition.edge_types.len(), 1);
            let et = &definition.edge_types[0];
            assert_eq!(et.label, "Posted");
            assert_eq!(et.from_labels, vec!["User"]);
            assert_eq!(et.to_labels, vec!["Post"]);
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

#[test]
fn graph_type_inline_edge_type_multiple() {
    let stmt = parse_statement(
        "CREATE GRAPH TYPE T { \
           (:User), (:Product), (:Order), \
           (:User)-[:Placed]->(:Order), \
           (:Order)-[:Contains]->(:Product) \
         }",
    )
    .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType { definition, .. } => {
            assert_eq!(definition.node_labels, vec!["Order", "Product", "User"]);
            assert_eq!(definition.edge_types.len(), 2);
            assert_eq!(definition.edge_types[0].label, "Placed");
            assert_eq!(definition.edge_types[0].from_labels, vec!["User"]);
            assert_eq!(definition.edge_types[0].to_labels, vec!["Order"]);
            assert_eq!(definition.edge_types[1].label, "Contains");
            assert_eq!(definition.edge_types[1].from_labels, vec!["Order"]);
            assert_eq!(definition.edge_types[1].to_labels, vec!["Product"]);
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

#[test]
fn graph_type_inline_edge_without_colon() {
    let stmt = parse_statement("CREATE GRAPH TYPE T { (User), (User)-[FOLLOWS]->(User) }")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType { definition, .. } => {
            assert_eq!(definition.edge_types.len(), 1);
            assert_eq!(definition.edge_types[0].label, "FOLLOWS");
            assert_eq!(definition.edge_types[0].from_labels, vec!["User"]);
            assert_eq!(definition.edge_types[0].to_labels, vec!["User"]);
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

#[test]
fn graph_type_inline_edge_mixed_with_bare_edges() {
    // Mix standard inline edge types with bare edge-label-only entries
    let stmt = parse_statement("CREATE GRAPH TYPE T { (:A), (:B), -[:LINK]->, (:A)-[:REL]->(:B) }")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType { definition, .. } => {
            assert_eq!(definition.edge_labels, vec!["LINK"]);
            assert_eq!(definition.edge_types.len(), 1);
            assert_eq!(definition.edge_types[0].label, "REL");
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

// ── SCHEMA (§12) ────────────────────────────────────────────────

#[test]
fn create_schema_parses_to_correct_statement() {
    let stmt = parse_statement("CREATE SCHEMA mySchema").unwrap_or_else(|e| panic!("parse: {e}"));
    assert!(
        matches!(stmt, Statement::CreateSchema { ref name, if_not_exists: false } if name == "mySchema")
    );
}

#[test]
fn drop_schema_parses_to_correct_statement() {
    let stmt = parse_statement("DROP SCHEMA mySchema").unwrap_or_else(|e| panic!("parse: {e}"));
    assert!(
        matches!(stmt, Statement::DropSchema { ref name, if_exists: false } if name == "mySchema")
    );
}

// ── IF [NOT] EXISTS (§12) ────────────────────────────────────────

#[test]
fn create_graph_if_not_exists_parses() {
    let stmt = parse_statement("CREATE GRAPH IF NOT EXISTS myGraph")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    assert!(matches!(
        stmt,
        Statement::CreateGraph {
            ref name,
            if_not_exists: true,
        } if name == "myGraph"
    ));
}

#[test]
fn drop_graph_if_exists_parses() {
    let stmt =
        parse_statement("DROP GRAPH IF EXISTS myGraph").unwrap_or_else(|e| panic!("parse: {e}"));
    assert!(matches!(
        stmt,
        Statement::DropGraph {
            ref name,
            if_exists: true,
        } if name == "myGraph"
    ));
}

#[test]
fn create_graph_type_if_not_exists_parses() {
    let stmt =
        parse_statement("CREATE GRAPH TYPE IF NOT EXISTS SocialNet { (:Person), -[:KNOWS]-> }")
            .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType {
            name,
            if_not_exists,
            ..
        } => {
            assert_eq!(name, "SocialNet");
            assert!(if_not_exists);
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

#[test]
fn drop_graph_type_if_exists_parses() {
    let stmt = parse_statement("DROP GRAPH TYPE IF EXISTS SocialNet")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    assert!(matches!(
        stmt,
        Statement::DropGraphType {
            ref name,
            if_exists: true,
        } if name == "SocialNet"
    ));
}

#[test]
fn create_or_replace_graph_type_parses() {
    let stmt = parse_statement("CREATE OR REPLACE GRAPH TYPE SocialNet { (:Person), -[:KNOWS]-> }")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType {
            name,
            or_replace,
            if_not_exists,
            ..
        } => {
            assert_eq!(name, "SocialNet");
            assert!(or_replace);
            assert!(!if_not_exists);
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

// ── PROPERTY GRAPH keyword alias (§12) ──────────────────────────

#[test]
fn create_property_graph_parses() {
    let stmt =
        parse_statement("CREATE PROPERTY GRAPH myGraph").unwrap_or_else(|e| panic!("parse: {e}"));
    assert!(matches!(stmt, Statement::CreateGraph { ref name, .. } if name == "myGraph"));
}

#[test]
fn drop_property_graph_parses() {
    let stmt =
        parse_statement("DROP PROPERTY GRAPH myGraph").unwrap_or_else(|e| panic!("parse: {e}"));
    assert!(matches!(stmt, Statement::DropGraph { ref name, .. } if name == "myGraph"));
}

#[test]
fn create_property_graph_type_parses() {
    let stmt = parse_statement("CREATE PROPERTY GRAPH TYPE MyType { (:Person), -[:KNOWS]-> }")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    assert!(matches!(stmt, Statement::CreateGraphType { ref name, .. } if name == "MyType"));
}

#[test]
fn drop_property_graph_type_parses() {
    let stmt =
        parse_statement("DROP PROPERTY GRAPH TYPE MyType").unwrap_or_else(|e| panic!("parse: {e}"));
    assert!(matches!(stmt, Statement::DropGraphType { ref name, .. } if name == "MyType"));
}

#[test]
fn create_or_replace_property_graph_type_parses() {
    let stmt = parse_statement("CREATE OR REPLACE PROPERTY GRAPH TYPE MyType { (:Person) }")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType {
            name, or_replace, ..
        } => {
            assert_eq!(name, "MyType");
            assert!(or_replace);
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

// ── LIKE / COPY OF (§12) ────────────────────────────────────────

#[test]
fn create_graph_type_like_parses() {
    let stmt = parse_statement("CREATE GRAPH TYPE NewType LIKE OldType")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType { name, source, .. } => {
            assert_eq!(name, "NewType");
            assert_eq!(source.as_deref(), Some("OldType"));
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

#[test]
fn create_graph_type_copy_of_parses() {
    let stmt = parse_statement("CREATE GRAPH TYPE NewType COPY OF OldType")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType { name, source, .. } => {
            assert_eq!(name, "NewType");
            assert_eq!(source.as_deref(), Some("OldType"));
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

#[test]
fn create_property_graph_type_like_parses() {
    let stmt = parse_statement("CREATE PROPERTY GRAPH TYPE NewType LIKE OldType")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    match &stmt {
        Statement::CreateGraphType { name, source, .. } => {
            assert_eq!(name, "NewType");
            assert_eq!(source.as_deref(), Some("OldType"));
        }
        other => panic!("expected CreateGraphType, got {other:?}"),
    }
}

#[test]
fn create_schema_if_not_exists_parses() {
    let stmt = parse_statement("CREATE SCHEMA IF NOT EXISTS mySchema")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    assert!(matches!(
        stmt,
        Statement::CreateSchema {
            ref name,
            if_not_exists: true,
        } if name == "mySchema"
    ));
}

#[test]
fn drop_schema_if_exists_parses() {
    let stmt =
        parse_statement("DROP SCHEMA IF EXISTS mySchema").unwrap_or_else(|e| panic!("parse: {e}"));
    assert!(matches!(
        stmt,
        Statement::DropSchema {
            ref name,
            if_exists: true,
        } if name == "mySchema"
    ));
}

// ── IS DIRECTED (§19.8) ───────────────────────────────────────────────────

#[test]
fn is_directed_parses_as_expr() {
    use gleaph_gql::ast::Expr;
    let stmt = parse_statement("MATCH (a)-[e:X]->(b) WHERE e IS DIRECTED RETURN e")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate: {e}"));
    let Statement::Query(q) = &stmt else {
        panic!("expected Query")
    };
    let where_expr = q.where_clause.as_ref().unwrap();
    assert!(matches!(
        where_expr,
        Expr::IsDirected { negated: false, .. }
    ));
}

#[test]
fn is_not_directed_parses_with_negated_flag() {
    use gleaph_gql::ast::Expr;
    let stmt = parse_statement("MATCH (a)-[e:X]->(b) WHERE e IS NOT DIRECTED RETURN e")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate: {e}"));
    let Statement::Query(q) = &stmt else {
        panic!("expected Query")
    };
    let where_expr = q.where_clause.as_ref().unwrap();
    assert!(matches!(where_expr, Expr::IsDirected { negated: true, .. }));
}

// ── Edge label expressions (§16.8 extension to edges) ──────────────────────────

#[test]
fn edge_label_or_parses_to_label_expr() {
    use gleaph_gql::ast::{LabelExpr, Statement};
    let stmt = parse_statement("MATCH (a)-[e:Bought|Picked]->(b) RETURN e")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap_or_else(|e| panic!("validate: {e}"));
    let Statement::Query(q) = &stmt else {
        panic!("expected Query")
    };
    let chain = &q.match_clauses[0].pattern.chain(0);
    assert!(
        chain.edge.label.is_none(),
        "label should be None for OR expression"
    );
    let le = chain
        .edge
        .label_expr
        .as_ref()
        .expect("label_expr should be Some");
    assert!(
        matches!(le, LabelExpr::Or(a, b)
            if matches!(a.as_ref(), LabelExpr::Name(n) if n == "Bought")
            && matches!(b.as_ref(), LabelExpr::Name(n) if n == "Picked")),
        "expected Or(Name(Bought), Name(Picked)), got {le:?}"
    );
}

#[test]
fn edge_label_three_way_or_parses_left_associative() {
    use gleaph_gql::ast::{LabelExpr, Statement};
    let stmt = parse_statement("MATCH (a)-[e:Bought|Picked|Favorited]->(b) RETURN e")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    let Statement::Query(q) = &stmt else {
        panic!("expected Query")
    };
    let chain = &q.match_clauses[0].pattern.chain(0);
    let le = chain
        .edge
        .label_expr
        .as_ref()
        .expect("label_expr should be Some");
    // Pratt parser: Bought|(Picked|Favorited)
    assert!(
        matches!(le, LabelExpr::Or(_, _)),
        "expected Or at top level, got {le:?}"
    );
}

#[test]
fn edge_label_not_parses_to_not_expr() {
    use gleaph_gql::ast::{LabelExpr, Statement};
    let stmt = parse_statement("MATCH (a)-[e:!Deleted]->(b) RETURN e")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    let Statement::Query(q) = &stmt else {
        panic!("expected Query")
    };
    let chain = &q.match_clauses[0].pattern.chain(0);
    let le = chain
        .edge
        .label_expr
        .as_ref()
        .expect("label_expr should be Some");
    assert!(
        matches!(le, LabelExpr::Not(inner) if matches!(inner.as_ref(), LabelExpr::Name(n) if n == "Deleted")),
        "expected Not(Name(Deleted)), got {le:?}"
    );
}

#[test]
fn edge_label_wildcard_parses() {
    use gleaph_gql::ast::{LabelExpr, Statement};
    let stmt =
        parse_statement("MATCH (a)-[e:%]->(b) RETURN e").unwrap_or_else(|e| panic!("parse: {e}"));
    let Statement::Query(q) = &stmt else {
        panic!("expected Query")
    };
    let chain = &q.match_clauses[0].pattern.chain(0);
    let le = chain
        .edge
        .label_expr
        .as_ref()
        .expect("label_expr should be Some");
    assert!(
        matches!(le, LabelExpr::Wildcard),
        "expected Wildcard, got {le:?}"
    );
}

#[test]
fn edge_single_label_still_uses_label_field() {
    use gleaph_gql::ast::Statement;
    let stmt = parse_statement("MATCH (a)-[e:KNOWS]->(b) RETURN e")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    let Statement::Query(q) = &stmt else {
        panic!("expected Query")
    };
    let chain = &q.match_clauses[0].pattern.chain(0);
    assert_eq!(
        chain.edge.label.as_deref(),
        Some("KNOWS"),
        "single label should use label field"
    );
    assert!(
        chain.edge.label_expr.is_none(),
        "label_expr should be None for single label"
    );
}

// ── Type annotation (§12) ────────────────────────────────────────────

#[test]
fn type_annotation_in_node_pattern() {
    use gleaph_gql::ast::{Statement, TypeExpr};
    let stmt =
        parse_statement("MATCH (n :: Person) RETURN n").unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap();
    let Statement::Query(q) = &stmt else {
        panic!("expected Query")
    };
    let start = &q.match_clauses[0].pattern.start;
    assert_eq!(start.var.as_deref(), Some("n"));
    assert!(
        start.labels.is_empty(),
        "labels should be empty when type_annotation is set"
    );
    assert_eq!(start.type_annotation, Some(TypeExpr::Name("Person".into())));
}

#[test]
fn type_annotation_union() {
    use gleaph_gql::ast::{Statement, TypeExpr};
    let stmt = parse_statement("MATCH (n :: Person | Company) RETURN n")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap();
    let Statement::Query(q) = &stmt else {
        panic!("expected Query")
    };
    let start = &q.match_clauses[0].pattern.start;
    assert_eq!(
        start.type_annotation,
        Some(TypeExpr::Union(
            Box::new(TypeExpr::Name("Person".into())),
            Box::new(TypeExpr::Name("Company".into())),
        ))
    );
}

#[test]
fn type_annotation_and_labels_rejected() {
    // Combining labels and type annotation should be a validation error.
    // The parser handles them as exclusive: `::` is only parsed when no `:label`.
    // But if someone tries `(n:Person :: PersonType)`, the `::` is not parsed
    // (the parser takes the `:Person` path), so the `:: PersonType` tokens will
    // cause a parse error (unexpected tokens before `)`).
    let result = parse_statement("MATCH (n:Person :: PersonType) RETURN n");
    assert!(
        result.is_err(),
        "should fail to parse labels + type annotation"
    );
}

#[test]
fn type_annotation_on_edge_pattern() {
    use gleaph_gql::ast::{Statement, TypeExpr};
    let stmt = parse_statement("MATCH (a)-[e :: KnowsType]->(b) RETURN e")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap();
    let Statement::Query(q) = &stmt else {
        panic!("expected Query")
    };
    let edge = &q.match_clauses[0].pattern.chain(0).edge;
    assert_eq!(edge.var.as_deref(), Some("e"));
    assert!(
        edge.label.is_none(),
        "label should be None when type_annotation is set"
    );
    assert_eq!(
        edge.type_annotation,
        Some(TypeExpr::Name("KnowsType".into()))
    );
}

#[test]
fn graph_type_inline_node_type() {
    use gleaph_gql::ast::Statement;
    let stmt = parse_statement("CREATE GRAPH TYPE Social { (PersonType :Person) }")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap();
    let Statement::CreateGraphType {
        name, definition, ..
    } = &stmt
    else {
        panic!("expected CreateGraphType")
    };
    assert_eq!(name, "Social");
    assert_eq!(definition.node_types.len(), 1);
    assert_eq!(definition.node_types[0].name, "PersonType");
    assert_eq!(definition.node_types[0].labels, vec!["Person".to_string()]);
}

#[test]
fn graph_type_inline_node_union_type() {
    use gleaph_gql::ast::Statement;
    let stmt = parse_statement(
        "CREATE GRAPH TYPE Social { (:Person), (:Company), (Entity :Person | :Company) }",
    )
    .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap();
    let Statement::CreateGraphType { definition, .. } = &stmt else {
        panic!("expected CreateGraphType")
    };
    assert_eq!(definition.node_types.len(), 1);
    assert_eq!(definition.node_types[0].name, "Entity");
    // Labels are sorted and deduped
    assert_eq!(
        definition.node_types[0].labels,
        vec!["Company".to_string(), "Person".to_string()]
    );
}

#[test]
fn graph_type_inline_node_type_with_properties() {
    use gleaph_gql::ast::{Statement, ValueType};
    let stmt = parse_statement(
        "CREATE GRAPH TYPE Social { (PersonType :Person { name :: TEXT NOT NULL, age :: INT, email :: TEXT }) }",
    )
    .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap();
    let Statement::CreateGraphType { definition, .. } = &stmt else {
        panic!("expected CreateGraphType")
    };
    assert_eq!(definition.node_types.len(), 1);
    let nt = &definition.node_types[0];
    assert_eq!(nt.name, "PersonType");
    assert_eq!(nt.properties.len(), 3);
    assert_eq!(nt.properties[0].name, "name");
    assert_eq!(nt.properties[0].value_type, ValueType::Text);
    assert!(nt.properties[0].required);
    assert_eq!(nt.properties[1].name, "age");
    assert_eq!(nt.properties[1].value_type, ValueType::Int32);
    assert!(!nt.properties[1].required);
    assert_eq!(nt.properties[2].name, "email");
    assert_eq!(nt.properties[2].value_type, ValueType::Text);
    assert!(!nt.properties[2].required);
}

#[test]
fn graph_type_inline_node_type_without_properties() {
    use gleaph_gql::ast::Statement;
    let stmt = parse_statement("CREATE GRAPH TYPE Social { (PersonType :Person) }")
        .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap();
    let Statement::CreateGraphType { definition, .. } = &stmt else {
        panic!("expected CreateGraphType")
    };
    assert_eq!(definition.node_types[0].properties.len(), 0);
}

// ── OPTIONAL { MATCH ... } block syntax (GQL standard) ──────────────────────

#[test]
fn optional_braced_match_parses() {
    // GQL standard: OPTIONAL { MATCH ... } block syntax.
    let cases = [
        // Basic braced form.
        "OPTIONAL { MATCH (a:User)-[:KNOWS]->(b) } RETURN a.name, b.name",
        // Braced form with parentheses instead of braces (GQL also allows this).
        "OPTIONAL ( MATCH (a:User)-[:KNOWS]->(b) ) RETURN a.name, b.name",
        // Non-optional MATCH followed by braced OPTIONAL.
        "MATCH (a:User) OPTIONAL { MATCH (a)-[:KNOWS]->(b) } WHERE a.name = 'Alice' RETURN a, b",
        // WITH continuation with braced OPTIONAL.
        "MATCH (a)-[:X]->(b) WITH a, b OPTIONAL { MATCH (b)-[:Y]->(c) } RETURN a, c",
        // Mixed: unbraced OPTIONAL MATCH still works alongside braced.
        "MATCH (a:User) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a, b",
    ];
    for gql in cases {
        let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    }
}

#[test]
fn optional_braced_match_unclosed_brace_rejected() {
    // Missing closing brace should produce a parse error.
    let result = parse_statement("OPTIONAL { MATCH (a:User)-[:KNOWS]->(b) RETURN a");
    assert!(
        result.is_err(),
        "unclosed brace in OPTIONAL block should fail"
    );
}

// ── NEXT pipeline syntax (GQL standard) ──────────────────────────────────────

#[test]
fn next_pipeline_parses() {
    // GQL NEXT keyword chains statements in a pipeline.
    let cases = [
        // Basic: MATCH ... RETURN ... NEXT MATCH ... RETURN ...
        "MATCH (a:User)-[:KNOWS]->(b) RETURN a.name AS name, b NEXT MATCH (b)-[:LIKES]->(c) RETURN name, c",
        // NEXT with ORDER BY + LIMIT on first stage.
        "MATCH (a:User) RETURN a ORDER BY a.name LIMIT 10 NEXT MATCH (a)-[:KNOWS]->(b) RETURN a, b",
        // NEXT with OPTIONAL MATCH in second stage.
        "MATCH (a:User) RETURN a NEXT OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a, b",
    ];
    for gql in cases {
        let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    }
}

#[test]
fn next_chains_two_statements() {
    // NEXT can chain any two statements (compound operator, like UNION).
    let stmt = parse_statement("MATCH (a:User) RETURN a NEXT RETURN 1");
    assert!(stmt.is_ok(), "NEXT should chain two statements");
}

// ── Simplified edge: label expressions ──────────────────────────────────────

#[test]
fn simplified_edge_label_expr_parses() {
    use gleaph_gql::ast::{LabelExpr, Statement};

    // OR: -/KNOWS|LIKES/->
    let stmt = parse_statement("MATCH (a)-/KNOWS|LIKES/->(b) RETURN a").unwrap();
    validate_statement(&stmt).unwrap();
    let Statement::Query(q) = &stmt else { panic!() };
    let edge = &q.match_clauses[0].pattern.chain(0).edge;
    assert!(edge.label.is_none());
    assert!(matches!(&edge.label_expr, Some(LabelExpr::Or(l, r))
        if matches!(l.as_ref(), LabelExpr::Name(n) if n == "KNOWS")
        && matches!(r.as_ref(), LabelExpr::Name(n) if n == "LIKES")));

    // NOT: -/!Deleted/->
    let stmt = parse_statement("MATCH (a)-/!Deleted/->(b) RETURN a").unwrap();
    validate_statement(&stmt).unwrap();
    let Statement::Query(q) = &stmt else { panic!() };
    let edge = &q.match_clauses[0].pattern.chain(0).edge;
    assert!(matches!(&edge.label_expr, Some(LabelExpr::Not(inner))
        if matches!(inner.as_ref(), LabelExpr::Name(n) if n == "Deleted")));

    // Wildcard: -/%/->
    let stmt = parse_statement("MATCH (a)-/%/->(b) RETURN a").unwrap();
    validate_statement(&stmt).unwrap();
    let Statement::Query(q) = &stmt else { panic!() };
    let edge = &q.match_clauses[0].pattern.chain(0).edge;
    assert!(matches!(&edge.label_expr, Some(LabelExpr::Wildcard)));

    // AND: -/A&B/->
    let stmt = parse_statement("MATCH (a)-/A&B/->(b) RETURN a").unwrap();
    validate_statement(&stmt).unwrap();
    let Statement::Query(q) = &stmt else { panic!() };
    let edge = &q.match_clauses[0].pattern.chain(0).edge;
    assert!(matches!(&edge.label_expr, Some(LabelExpr::And(..))));

    // Incoming with OR: <-/L|M/-
    let stmt = parse_statement("MATCH (a)<-/L|M/-(b) RETURN a").unwrap();
    validate_statement(&stmt).unwrap();

    // Either with OR: <-/L|M/->
    let stmt = parse_statement("MATCH (a)<-/L|M/->(b) RETURN a").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn simplified_edge_quantifier_parses() {
    use gleaph_gql::ast::{PathLength, Statement};

    let cases: &[(&str, PathLength)] = &[
        (
            "MATCH (a)-/KNOWS*/->(b) RETURN a",
            PathLength::Range { min: 1, max: 10 },
        ),
        (
            "MATCH (a)-/KNOWS+/->(b) RETURN a",
            PathLength::Range { min: 1, max: 10 },
        ),
        (
            "MATCH (a)-/KNOWS*1..3/->(b) RETURN a",
            PathLength::Range { min: 1, max: 3 },
        ),
        ("MATCH (a)-/KNOWS{2}/->(b) RETURN a", PathLength::Fixed(2)),
        (
            "MATCH (a)-/KNOWS{2,4}/->(b) RETURN a",
            PathLength::Range { min: 2, max: 4 },
        ),
    ];
    for (gql, expected_len) in cases {
        let stmt = parse_statement(gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
        let Statement::Query(q) = &stmt else {
            panic!("expected Query for {gql}")
        };
        let edge = &q.match_clauses[0].pattern.chain(0).edge;
        assert_eq!(edge.label, Some("KNOWS".into()), "label for {gql}");
        assert_eq!(&edge.length, expected_len, "length for {gql}");
    }
}

#[test]
fn simplified_edge_label_expr_with_quantifier() {
    use gleaph_gql::ast::{LabelExpr, PathLength, Statement};

    // -/KNOWS|LIKES*1..3/->
    let stmt = parse_statement("MATCH (a)-/KNOWS|LIKES*1..3/->(b) RETURN a").unwrap();
    validate_statement(&stmt).unwrap();
    let Statement::Query(q) = &stmt else { panic!() };
    let edge = &q.match_clauses[0].pattern.chain(0).edge;
    assert!(edge.label.is_none());
    assert!(matches!(&edge.label_expr, Some(LabelExpr::Or(..))));
    assert_eq!(edge.length, PathLength::Range { min: 1, max: 3 });
}

// ── GQL §16.7: Mutual exclusivity of {props} and inline WHERE ──────────────

#[test]
fn node_props_hint_and_inline_where_rejected() {
    let gql = r#"MATCH (n:User {name: 'Alice'} WHERE n.age > 25) RETURN n"#;
    let err = parse_statement(gql)
        .and_then(|s| validate_statement(&s).map(|()| s))
        .expect_err("node {props} + WHERE should be rejected");
    assert!(
        err.to_string().contains("property map"),
        "expected property map error, got: {err}"
    );
}

#[test]
fn edge_props_and_inline_where_rejected() {
    let gql = r#"MATCH (a)-[e:K {w: 1} WHERE e.x > 0]->(b) RETURN b"#;
    let err = parse_statement(gql)
        .and_then(|s| validate_statement(&s).map(|()| s))
        .expect_err("edge {props} + WHERE should be rejected");
    assert!(
        err.to_string().contains("property map"),
        "expected property map error, got: {err}"
    );
}

// ── Parameter type annotation parsing (GQL §21.3) ────────────────────────────

#[test]
fn parameter_type_annotation_parses() {
    use gleaph_gql::ast::ValueType;
    let stmt = parse_statement("RETURN $x :: INT").unwrap();
    if let Statement::Query(q) = &stmt {
        match &q.return_clause.items[0].expr {
            Expr::Parameter {
                name,
                type_annotation,
            } => {
                assert_eq!(name, "x");
                assert_eq!(*type_annotation, Some(vec![ValueType::Int32]));
            }
            other => panic!("expected Parameter, got: {other:?}"),
        }
    } else {
        panic!("expected Query");
    }
}

#[test]
fn parameter_no_annotation_parses() {
    let stmt = parse_statement("RETURN $x").unwrap();
    if let Statement::Query(q) = &stmt {
        match &q.return_clause.items[0].expr {
            Expr::Parameter {
                name,
                type_annotation,
            } => {
                assert_eq!(name, "x");
                assert_eq!(*type_annotation, None);
            }
            other => panic!("expected Parameter, got: {other:?}"),
        }
    } else {
        panic!("expected Query");
    }
}

#[test]
fn parameter_type_annotation_all_types_parse() {
    use gleaph_gql::ast::ValueType;
    let cases = [
        ("$x :: INTEGER", ValueType::Int32),
        ("$x :: INT", ValueType::Int32),
        ("$x :: BIGINT", ValueType::Int64),
        ("$x :: FLOAT", ValueType::Float32),
        ("$x :: FLOAT32", ValueType::Float32),
        ("$x :: REAL", ValueType::Float32),
        ("$x :: DOUBLE", ValueType::Float64),
        ("$x :: FLOAT64", ValueType::Float64),
        ("$x :: STRING", ValueType::Text),
        ("$x :: VARCHAR", ValueType::Text),
        ("$x :: TEXT", ValueType::Text),
        ("$x :: BOOLEAN", ValueType::Bool),
        ("$x :: BOOL", ValueType::Bool),
        ("$x :: TIMESTAMP", ValueType::Timestamp),
        ("$x :: LIST", ValueType::List),
        ("$x :: NULL", ValueType::Null),
    ];
    for (expr_str, expected_type) in cases {
        let gql = format!("RETURN {expr_str}");
        let stmt = parse_statement(&gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
        if let Statement::Query(q) = &stmt {
            match &q.return_clause.items[0].expr {
                Expr::Parameter {
                    type_annotation, ..
                } => {
                    assert_eq!(
                        *type_annotation,
                        Some(vec![expected_type]),
                        "mismatch for {expr_str}"
                    );
                }
                other => panic!("expected Parameter for {expr_str}, got: {other:?}"),
            }
        }
    }
}

#[test]
fn parameter_unknown_type_annotation_errors() {
    let result = parse_statement("RETURN $x :: FOOBAR");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("unknown parameter type"),
        "expected type error, got: {err}"
    );
}

#[test]
fn cast_with_value_type_enum_parses() {
    use gleaph_gql::ast::ValueType;
    let stmt = parse_statement("RETURN CAST(42 AS FLOAT)").unwrap();
    if let Statement::Query(q) = &stmt {
        match &q.return_clause.items[0].expr {
            Expr::Cast { target_type, .. } => {
                assert_eq!(*target_type, ValueType::Float32);
            }
            other => panic!("expected Cast, got: {other:?}"),
        }
    }
}

#[test]
fn cast_unknown_type_errors() {
    let result = parse_statement("RETURN CAST(42 AS FOOBAR)");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("expected type name") || err.contains("unknown CAST type"),
        "expected type error, got: {err}"
    );
}

#[test]
fn is_type_with_value_type_enum_parses() {
    use gleaph_gql::ast::ValueType;
    let stmt = parse_statement("RETURN 42 IS :: INTEGER").unwrap();
    if let Statement::Query(q) = &stmt {
        match &q.return_clause.items[0].expr {
            Expr::IsType {
                value_type,
                type_name,
                ..
            } => {
                assert_eq!(*value_type, Some(ValueType::Int32));
                assert_eq!(type_name, "INTEGER");
            }
            other => panic!("expected IsType, got: {other:?}"),
        }
    }
}

#[test]
fn is_type_node_type_name_parses() {
    let stmt = parse_statement("RETURN 1 IS :: PersonType").unwrap();
    if let Statement::Query(q) = &stmt {
        match &q.return_clause.items[0].expr {
            Expr::IsType {
                value_type,
                type_name,
                ..
            } => {
                assert_eq!(*value_type, None);
                assert_eq!(type_name, "PersonType");
            }
            other => panic!("expected IsType, got: {other:?}"),
        }
    }
}

// ── Inline edge type definitions (§18.3) ─────────────────────────────────────

#[test]
fn graph_type_edge_type_basic() {
    use gleaph_gql::ast::Statement;
    let stmt =
        parse_statement("CREATE GRAPH TYPE Social { (:Person), (:Person)-[:KNOWS]->(:Person) }")
            .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap();
    let Statement::CreateGraphType { definition, .. } = &stmt else {
        panic!("expected CreateGraphType")
    };
    assert_eq!(definition.edge_types.len(), 1);
    let et = &definition.edge_types[0];
    assert_eq!(et.label, "KNOWS");
    assert_eq!(et.from_labels, vec!["Person".to_string()]);
    assert_eq!(et.to_labels, vec!["Person".to_string()]);
    assert!(et.properties.is_empty());
}

#[test]
fn graph_type_edge_type_with_properties() {
    use gleaph_gql::ast::{Statement, ValueType};
    let stmt = parse_statement(
        "CREATE GRAPH TYPE Social { (:Person), (:Company), (:Person)-[:WORKS_AT { since :: INT, role :: TEXT NOT NULL }]->(:Company) }",
    )
    .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap();
    let Statement::CreateGraphType { definition, .. } = &stmt else {
        panic!("expected CreateGraphType")
    };
    assert_eq!(definition.edge_types.len(), 1);
    let et = &definition.edge_types[0];
    assert_eq!(et.label, "WORKS_AT");
    assert_eq!(et.from_labels, vec!["Person".to_string()]);
    assert_eq!(et.to_labels, vec!["Company".to_string()]);
    assert_eq!(et.properties.len(), 2);
    assert_eq!(et.properties[0].name, "since");
    assert_eq!(et.properties[0].value_type, ValueType::Int32);
    assert!(!et.properties[0].required);
    assert_eq!(et.properties[1].name, "role");
    assert_eq!(et.properties[1].value_type, ValueType::Text);
    assert!(et.properties[1].required);
}

#[test]
fn graph_type_edge_type_multiple_endpoints() {
    use gleaph_gql::ast::Statement;
    let stmt = parse_statement(
        "CREATE GRAPH TYPE Social { (:Person), (:Contractor), (:Company), (:Startup), (:Person | :Contractor)-[:WORKS_AT]->(:Company | :Startup) }",
    )
    .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap();
    let Statement::CreateGraphType { definition, .. } = &stmt else {
        panic!("expected CreateGraphType")
    };
    assert_eq!(definition.edge_types.len(), 1);
    let et = &definition.edge_types[0];
    assert_eq!(
        et.from_labels,
        vec!["Contractor".to_string(), "Person".to_string()]
    );
    assert_eq!(
        et.to_labels,
        vec!["Company".to_string(), "Startup".to_string()]
    );
}

#[test]
fn graph_type_mixed_node_and_edge_types() {
    use gleaph_gql::ast::Statement;
    let stmt = parse_statement(
        "CREATE GRAPH TYPE Social { (:Person), (:Company), -[:KNOWS]->, -[:WORKS_AT]->, (PersonType :Person { name :: TEXT }), (:Person)-[:KNOWS]->(:Person) }",
    )
    .unwrap_or_else(|e| panic!("parse: {e}"));
    validate_statement(&stmt).unwrap();
    let Statement::CreateGraphType { definition, .. } = &stmt else {
        panic!("expected CreateGraphType")
    };
    assert_eq!(definition.node_types.len(), 1);
    assert_eq!(definition.node_types[0].name, "PersonType");
    assert_eq!(definition.edge_types.len(), 1);
    assert_eq!(definition.edge_types[0].label, "KNOWS");
}

// ── DESCRIBE GRAPH TYPE ─────────────────────────────────────────────────────

#[test]
fn describe_graph_type_parses() {
    let stmt = parse_statement("DESCRIBE GRAPH TYPE Social").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(stmt, Statement::DescribeGraphType("Social".into()));
}

// ── SHOW / CREATE INDEX / GRANT / REVOKE / ANALYZE ──────────────────────────

#[test]
fn show_stats_parses() {
    use gleaph_gql::ast::ShowTarget;
    let stmt = parse_statement("SHOW STATS").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(stmt, Statement::Show(ShowTarget::Stats));
}

#[test]
fn show_planner_stats_parses() {
    use gleaph_gql::ast::ShowTarget;
    let stmt = parse_statement("SHOW PLANNER STATS").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(stmt, Statement::Show(ShowTarget::PlannerStats));
}

#[test]
fn show_indexes_parses() {
    use gleaph_gql::ast::ShowTarget;
    for kw in &["INDEXES", "INDICES"] {
        let stmt = parse_statement(&format!("SHOW {kw}")).unwrap();
        validate_statement(&stmt).unwrap();
        assert_eq!(stmt, Statement::Show(ShowTarget::Indexes));
    }
}

#[test]
fn show_grants_parses() {
    use gleaph_gql::ast::ShowTarget;
    let stmt = parse_statement("SHOW GRANTS").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(stmt, Statement::Show(ShowTarget::Grants));
}

#[test]
fn show_metrics_parses() {
    use gleaph_gql::ast::ShowTarget;
    let stmt = parse_statement("SHOW METRICS").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(stmt, Statement::Show(ShowTarget::Metrics));
}

#[test]
fn show_schemas_parses() {
    use gleaph_gql::ast::ShowTarget;
    let stmt = parse_statement("SHOW SCHEMAS").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(stmt, Statement::Show(ShowTarget::Schemas));
}

#[test]
fn show_graph_types_parses() {
    use gleaph_gql::ast::ShowTarget;
    let stmt = parse_statement("SHOW GRAPH TYPES").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(stmt, Statement::Show(ShowTarget::GraphTypes));
}

#[test]
fn show_quota_parses() {
    use gleaph_gql::ast::ShowTarget;
    let stmt = parse_statement("SHOW QUOTA").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(stmt, Statement::Show(ShowTarget::Quota));
}

#[test]
fn show_aliases_parses() {
    use gleaph_gql::ast::ShowTarget;
    let stmt = parse_statement("SHOW ALIASES").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(stmt, Statement::Show(ShowTarget::Aliases));
}

#[test]
fn show_prepared_parses() {
    use gleaph_gql::ast::ShowTarget;
    let stmt = parse_statement("SHOW PREPARED").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(stmt, Statement::Show(ShowTarget::Prepared));
}

#[test]
fn show_unknown_target_fails() {
    let err = parse_statement("SHOW BANANAS").unwrap_err();
    assert!(err.to_string().contains("unknown SHOW target"));
}

#[test]
fn create_index_vertex_parses() {
    let stmt = parse_statement("CREATE INDEX ON :User(name)").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(
        stmt,
        Statement::CreateIndex {
            entity_type: gleaph_types::EntityType::Vertex,
            property_name: "name".into(),
        }
    );
}

#[test]
fn create_index_edge_parses() {
    let stmt = parse_statement("CREATE INDEX ON -[:KNOWS](since)").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(
        stmt,
        Statement::CreateIndex {
            entity_type: gleaph_types::EntityType::Edge,
            property_name: "since".into(),
        }
    );
}

#[test]
fn drop_index_vertex_parses() {
    let stmt = parse_statement("DROP INDEX ON :User(name)").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(
        stmt,
        Statement::DropIndex {
            entity_type: gleaph_types::EntityType::Vertex,
            property_name: "name".into(),
        }
    );
}

#[test]
fn grant_write_parses() {
    let stmt = parse_statement("GRANT WRITE ON GRAPH TO 'aaaaa-aa'").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(
        stmt,
        Statement::Grant {
            level: gleaph_types::AccessLevel::Write,
            principal: "aaaaa-aa".into(),
        }
    );
}

#[test]
fn grant_admin_parses() {
    let stmt = parse_statement("GRANT ADMIN ON GRAPH TO 'aaaaa-aa'").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(
        stmt,
        Statement::Grant {
            level: gleaph_types::AccessLevel::Admin,
            principal: "aaaaa-aa".into(),
        }
    );
}

#[test]
fn revoke_access_parses() {
    let stmt = parse_statement("REVOKE ACCESS ON GRAPH FROM 'aaaaa-aa'").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(
        stmt,
        Statement::Revoke {
            principal: "aaaaa-aa".into(),
        }
    );
}

#[test]
fn analyze_parses() {
    let stmt = parse_statement("ANALYZE").unwrap();
    validate_statement(&stmt).unwrap();
    assert_eq!(stmt, Statement::Analyze);
}

// ── CALL procedure ────────────────────────────────────────────────────

#[test]
fn call_bfs_procedure_parses() {
    let stmt = parse_statement("CALL bfs(42, {max_depth: 5}) YIELD vertex_id, distance").unwrap();
    validate_statement(&stmt).unwrap();
    if let Statement::CallProcedure(call) = &stmt {
        assert_eq!(call.procedure, "bfs");
        assert_eq!(call.args.len(), 2);
        assert_eq!(
            call.yield_cols,
            Some(vec!["vertex_id".to_string(), "distance".to_string()])
        );
    } else {
        panic!("expected CallProcedure, got {stmt:?}");
    }
}

#[test]
fn call_pagerank_no_args_parses() {
    let stmt = parse_statement("CALL pagerank({damping: 0.85}) YIELD vertex_id, score").unwrap();
    validate_statement(&stmt).unwrap();
    if let Statement::CallProcedure(call) = &stmt {
        assert_eq!(call.procedure, "pagerank");
        assert_eq!(call.args.len(), 1);
        assert_eq!(
            call.yield_cols,
            Some(vec!["vertex_id".to_string(), "score".to_string()])
        );
    } else {
        panic!("expected CallProcedure, got {stmt:?}");
    }
}

#[test]
fn call_procedure_without_yield_parses() {
    let stmt = parse_statement("CALL bfs(0)").unwrap();
    validate_statement(&stmt).unwrap();
    if let Statement::CallProcedure(call) = &stmt {
        assert_eq!(call.procedure, "bfs");
        assert!(call.yield_cols.is_none());
    } else {
        panic!("expected CallProcedure, got {stmt:?}");
    }
}

#[test]
fn call_subquery_still_works() {
    // Ensure CALL (<vars>) { body } still parses
    let stmt = parse_statement("CALL (x) { MATCH (n) RETURN n AS x }").unwrap();
    validate_statement(&stmt).unwrap();
    assert!(matches!(stmt, Statement::Call(_)));
}

#[test]
fn optional_call_subquery_parses() {
    let stmt = parse_statement("OPTIONAL CALL (x) { MATCH (n) RETURN n AS x }").unwrap();
    validate_statement(&stmt).unwrap();
    if let Statement::Call(c) = &stmt {
        assert!(c.optional);
    } else {
        panic!("expected Call, got {stmt:?}");
    }
}

#[test]
fn call_procedure_with_parameter_arg_parses() {
    let stmt =
        parse_statement("CALL bfs($start, {max_depth: $depth}) YIELD vertex_id, distance").unwrap();
    validate_statement(&stmt).unwrap();
    if let Statement::CallProcedure(call) = &stmt {
        assert_eq!(call.procedure, "bfs");
        assert_eq!(call.args.len(), 2);
    } else {
        panic!("expected CallProcedure, got {stmt:?}");
    }
}

#[test]
fn call_procedure_with_arithmetic_arg_parses() {
    let stmt =
        parse_statement("CALL bfs(1 + 2, {max_depth: 3 * 2}) YIELD vertex_id, distance").unwrap();
    validate_statement(&stmt).unwrap();
    if let Statement::CallProcedure(call) = &stmt {
        assert_eq!(call.args.len(), 2);
    } else {
        panic!("expected CallProcedure, got {stmt:?}");
    }
}

// ── Parenthesized subpath patterns (§16.7) ──────────────────────────────────

#[test]
fn parse_subpath_fixed_quantifier() {
    use gleaph_gql::ast::{PathLength, PatternElement, Statement};
    let stmt = parse_statement("MATCH (a)((x)-[:E]->(y)){3}(b) RETURN a, b").unwrap();
    validate_statement(&stmt).unwrap();
    if let Statement::Query(q) = &stmt {
        let m = &q.match_clauses[0].pattern;
        // Should have 1 SubPath element
        assert_eq!(m.elements.len(), 1);
        match &m.elements[0] {
            PatternElement::SubPath {
                inner_start,
                inner_elements,
                quantifier,
                ..
            } => {
                assert_eq!(inner_start.var.as_deref(), Some("x"));
                assert_eq!(inner_elements.len(), 1);
                assert_eq!(*quantifier, PathLength::Fixed(3));
            }
            other => panic!("expected SubPath, got {other:?}"),
        }
    } else {
        panic!("expected Query");
    }
}

#[test]
fn parse_subpath_range_quantifier() {
    use gleaph_gql::ast::{PathLength, PatternElement, Statement};
    let stmt = parse_statement("MATCH (a)((x)-[:E]->(y)){1,3}(b) RETURN a, b").unwrap();
    validate_statement(&stmt).unwrap();
    if let Statement::Query(q) = &stmt {
        let m = &q.match_clauses[0].pattern;
        assert_eq!(m.elements.len(), 1);
        match &m.elements[0] {
            PatternElement::SubPath { quantifier, .. } => {
                assert_eq!(*quantifier, PathLength::Range { min: 1, max: 3 });
            }
            other => panic!("expected SubPath, got {other:?}"),
        }
    } else {
        panic!("expected Query");
    }
}

#[test]
fn parse_subpath_no_quantifier_defaults_to_fixed_1() {
    use gleaph_gql::ast::{PathLength, PatternElement, Statement};
    let stmt = parse_statement("MATCH (a)((x)-[:E]->(y))(b) RETURN a, b").unwrap();
    validate_statement(&stmt).unwrap();
    if let Statement::Query(q) = &stmt {
        let m = &q.match_clauses[0].pattern;
        assert_eq!(m.elements.len(), 1);
        match &m.elements[0] {
            PatternElement::SubPath { quantifier, .. } => {
                assert_eq!(*quantifier, PathLength::Fixed(1));
            }
            other => panic!("expected SubPath, got {other:?}"),
        }
    } else {
        panic!("expected Query");
    }
}

// ── IS LABELED / IS SOURCE OF / IS DESTINATION OF / predicates ───────────────

#[test]
fn is_labeled_parses() {
    let stmt = parse_statement("MATCH (n) WHERE n IS LABELED :User RETURN n").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn is_not_labeled_parses() {
    let stmt = parse_statement("MATCH (n) WHERE n IS NOT LABELED :Admin RETURN n").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn is_labelled_british_spelling_parses() {
    let stmt = parse_statement("MATCH (n) WHERE n IS LABELLED :User RETURN n").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn is_source_of_parses() {
    let stmt = parse_statement("MATCH (a)-[e]->(b) WHERE a IS SOURCE OF e RETURN a").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn is_not_source_of_parses() {
    let stmt = parse_statement("MATCH (a)-[e]->(b) WHERE a IS NOT SOURCE OF e RETURN a").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn is_destination_of_parses() {
    let stmt = parse_statement("MATCH (a)-[e]->(b) WHERE b IS DESTINATION OF e RETURN b").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn is_not_destination_of_parses() {
    let stmt =
        parse_statement("MATCH (a)-[e]->(b) WHERE b IS NOT DESTINATION OF e RETURN b").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn all_different_parses() {
    let stmt =
        parse_statement("MATCH (a)-[e1]->(b)-[e2]->(c) RETURN ALL_DIFFERENT(a, b, c)").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn same_parses() {
    let stmt = parse_statement("MATCH (a)-[e]->(b) RETURN SAME(a.name, b.name)").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn property_exists_parses() {
    let stmt = parse_statement("MATCH (n) WHERE PROPERTY_EXISTS(n, 'name') RETURN n").unwrap();
    validate_statement(&stmt).unwrap();
}

// ── IS TRUE / IS FALSE / IS UNKNOWN (§20.1) ─────────────────────────────────

#[test]
fn is_true_parses() {
    let stmt = parse_statement("MATCH (n) WHERE (n.active = TRUE) IS TRUE RETURN n").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn is_not_false_parses() {
    let stmt = parse_statement("MATCH (n) WHERE (n.x > 0) IS NOT FALSE RETURN n").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn is_unknown_parses() {
    let stmt = parse_statement("MATCH (n) WHERE n.age IS UNKNOWN RETURN n").unwrap();
    validate_statement(&stmt).unwrap();
}

// ── VALUE subquery (§20.6) ───────────────────────────────────────────────────

#[test]
fn value_subquery_parses() {
    let stmt = parse_statement(
        "MATCH (a:User) RETURN a.name, VALUE { MATCH (a)-[:KNOWS]->(b) RETURN COUNT(*) } AS cnt",
    )
    .unwrap();
    validate_statement(&stmt).unwrap();
}

// ── LET ... IN (§20.5) ──────────────────────────────────────────────────────

#[test]
fn let_in_parses() {
    let stmt =
        parse_statement("MATCH (n:User) RETURN LET x = n.score * 2 IN x + 1 END AS val").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn let_in_multiple_bindings_parses() {
    let stmt =
        parse_statement("MATCH (n:User) RETURN LET x = 1, y = 2 IN x + y END AS val").unwrap();
    validate_statement(&stmt).unwrap();
}

// ── FILTER statement (§14.6) ─────────────────────────────────────────────────

#[test]
fn filter_statement_parses() {
    let stmt =
        parse_statement("MATCH (n:User) WHERE n.score > 10 FILTER n.name = 'Alice'").unwrap();
    validate_statement(&stmt).unwrap();
}

// ── LET statement (§14.7) ───────────────────────────────────────────────────

#[test]
fn let_statement_parses() {
    let stmt = parse_statement("MATCH (n:User) LET x = n.score * 2 RETURN n.name, x").unwrap();
    validate_statement(&stmt).unwrap();
}

// ── FINISH (§14.10) ─────────────────────────────────────────────────────────

#[test]
fn finish_parses() {
    let stmt = parse_statement("MATCH (n:User) RETURN FINISH").unwrap();
    validate_statement(&stmt).unwrap();
}

// ── OTHERWISE (§14.2) ────────────────────────────────────────────────────────

#[test]
fn otherwise_parses() {
    let stmt =
        parse_statement("MATCH (n:User) RETURN n.name OTHERWISE MATCH (m:User) RETURN m.name")
            .unwrap();
    validate_statement(&stmt).unwrap();
}

// ── Parenthesized subpath (§16.7) additional tests ───────────────────────────

#[test]
fn subpath_acyclic_mode_parses() {
    let stmt = parse_statement("MATCH ACYCLIC (a:N)((x)-[:E]->(y)){1,3}(b:N) RETURN a, b").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn subpath_trailing_node_with_label_parses() {
    let stmt = parse_statement("MATCH (a:A)((x)-[:E]->(y)){2}(b:B) RETURN a, b").unwrap();
    validate_statement(&stmt).unwrap();
}

// ── ANY SHORTEST / ALL PATHS (§16.6) ────────────────────────────────────────

#[test]
fn any_shortest_parses() {
    let stmt = parse_statement(
        "MATCH ANY SHORTEST p = (a)-[:KNOWS*1..3]->(b) WHERE a.name = 'Alice' RETURN p",
    )
    .unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn all_paths_parses() {
    let stmt = parse_statement(
        "MATCH ALL PATHS p = (a)-[:KNOWS*1..3]->(b) WHERE a.name = 'Alice' RETURN p",
    )
    .unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn all_paths_with_trail_mode_parses() {
    let stmt = parse_statement("MATCH TRAIL ALL PATHS p = (a)-[:E*1..4]->(b) RETURN a, b").unwrap();
    validate_statement(&stmt).unwrap();
}

// ── Character string type constraints (STRING/VARCHAR/CHAR with lengths) ──────

#[test]
fn cast_string_with_max_length_parses() {
    let stmt = parse_statement("RETURN CAST('hello' AS STRING(10))").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn cast_string_with_min_max_length_parses() {
    let stmt = parse_statement("RETURN CAST('hello' AS STRING(3, 20))").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn cast_varchar_with_max_length_parses() {
    let stmt = parse_statement("RETURN CAST('hello' AS VARCHAR(10))").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn cast_char_with_fixed_length_parses() {
    let stmt = parse_statement("RETURN CAST('hello' AS CHAR(5))").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn bare_string_varchar_char_parse_as_text() {
    // Without parenthesised length, these should parse as unconstrained Text.
    for kw in ["STRING", "VARCHAR", "CHAR"] {
        let gql = format!("RETURN CAST(123 AS {kw})");
        let stmt = parse_statement(&gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    }
}

#[test]
fn string_length_zero_is_rejected() {
    let err = parse_statement("RETURN CAST('x' AS STRING(0))");
    assert!(err.is_err(), "STRING(0) should be rejected");
}

#[test]
fn char_length_zero_is_rejected() {
    let err = parse_statement("RETURN CAST('x' AS CHAR(0))");
    assert!(err.is_err(), "CHAR(0) should be rejected");
}

#[test]
fn string_min_exceeds_max_is_rejected() {
    let err = parse_statement("RETURN CAST('x' AS STRING(10, 5))");
    assert!(
        err.is_err(),
        "STRING(10, 5) where min>max should be rejected"
    );
}

#[test]
fn param_annotation_with_string_length() {
    let stmt = parse_statement("RETURN $name :: STRING(50)").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn param_annotation_with_char_length() {
    let stmt = parse_statement("RETURN $code :: CHAR(3)").unwrap();
    validate_statement(&stmt).unwrap();
}

// ── Byte string type constraints (BYTES/BINARY/VARBINARY with lengths) ────────

#[test]
fn cast_bytes_with_max_length_parses() {
    let stmt = parse_statement("RETURN CAST('4142' AS BYTES(10))").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn cast_bytes_with_min_max_length_parses() {
    let stmt = parse_statement("RETURN CAST('4142' AS BYTES(1, 10))").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn cast_varbinary_with_max_length_parses() {
    let stmt = parse_statement("RETURN CAST('4142' AS VARBINARY(10))").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn cast_binary_with_fixed_length_parses() {
    let stmt = parse_statement("RETURN CAST('4142' AS BINARY(2))").unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn bare_bytes_binary_varbinary_parse_as_bytes() {
    for kw in ["BYTES", "BINARY", "VARBINARY"] {
        let gql = format!("RETURN CAST('41' AS {kw})");
        let stmt = parse_statement(&gql).unwrap_or_else(|e| panic!("parse '{gql}': {e}"));
        validate_statement(&stmt).unwrap_or_else(|e| panic!("validate '{gql}': {e}"));
    }
}

#[test]
fn bytes_length_zero_is_rejected() {
    assert!(parse_statement("RETURN CAST('41' AS BYTES(0))").is_err());
}

#[test]
fn bytes_min_exceeds_max_is_rejected() {
    assert!(parse_statement("RETURN CAST('41' AS BYTES(10, 5))").is_err());
}
