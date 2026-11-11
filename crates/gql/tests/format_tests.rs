#![cfg(feature = "format")]

use gleaph_gql::{
    ClauseBreakPolicy, FormatError, FormatOptions, ItemBreakPolicy, KeywordCase, format_query,
};

#[test]
fn formats_standard_query_and_is_stable() {
    let options = FormatOptions::default();
    let formatted = format_query("MATCH (n) RETURN n", &options).unwrap();
    assert_eq!(formatted, "MATCH (n)\nRETURN\n  n");
    assert_eq!(format_query(&formatted, &options).unwrap(), formatted);
}

#[test]
fn parser_errors_are_not_silently_formatted() {
    let error = format_query("MATCH (", &FormatOptions::default()).unwrap_err();
    assert!(matches!(error, FormatError::Parse(_)));
}

#[test]
fn unsupported_ast_shapes_return_explicit_errors() {
    // Byte-string values are a documented formatter deferral; the point of this test is that a
    // deferred shape reports `Unsupported` instead of inventing text.
    let error = format_query("RETURN X'00FF'", &FormatOptions::default()).unwrap_err();
    assert!(matches!(error, FormatError::Unsupported(_)));
}

#[test]
fn empty_indentation_is_rejected() {
    let mut options = FormatOptions::default();
    options.indentation.clear();
    assert!(matches!(
        format_query("MATCH (n) RETURN n", &options),
        Err(FormatError::InvalidOptions(_))
    ));
}

#[test]
fn formatting_options_change_presentation_policy() {
    let options = FormatOptions {
        clause_breaks: ClauseBreakPolicy::Compact,
        ..FormatOptions::default()
    };
    let formatted = format_query("MATCH (n) RETURN n", &options).unwrap();
    assert_eq!(formatted, "MATCH (n) RETURN n");
}

#[test]
fn compact_formatting_wraps_projection_items_at_line_width() {
    assert_eq!(FormatOptions::default().line_width, 100);
    let options = FormatOptions {
        clause_breaks: ClauseBreakPolicy::Compact,
        result_item_breaks: ItemBreakPolicy::Compact,
        line_width: 30,
        ..FormatOptions::default()
    };
    let formatted = format_query(
        "MATCH (n) RETURN n.first_name AS first_name, n.last_name AS last_name",
        &options,
    )
    .unwrap();
    assert_eq!(
        formatted,
        "MATCH (n) RETURN\n  n.first_name AS first_name,\n  n.last_name AS last_name"
    );
    assert!(
        formatted
            .lines()
            .all(|line| line.len() <= options.line_width)
    );
}

#[test]
fn match_where_wraps_as_a_block_when_match_exceeds_line_width() {
    let options = FormatOptions::default();
    let formatted = format_query(
        "MATCH (u:User {demo_id: 1})<-[e:IN_HOME_FEED]-(p:Post)<-[:POSTED]-(author:User) WHERE p.is_public = TRUE",
        &options,
    )
    .unwrap();
    assert_eq!(
        formatted,
        "MATCH (u:User {demo_id: 1})<-[e:IN_HOME_FEED]-(p:Post)<-[:POSTED]-(author:User)\n  WHERE p.is_public = TRUE"
    );
    assert!(
        formatted
            .lines()
            .all(|line| line.len() <= options.line_width)
    );
    assert_eq!(format_query(&formatted, &options).unwrap(), formatted);
}

#[test]
fn lowercase_keywords_round_trip() {
    let options = FormatOptions {
        keyword_case: KeywordCase::Lower,
        ..FormatOptions::default()
    };
    let formatted = format_query("MATCH (n) RETURN n", &options).unwrap();
    assert_eq!(formatted, "match (n)\nreturn\n  n");
    assert_eq!(format_query(&formatted, &options).unwrap(), formatted);
}

#[test]
fn arithmetic_precedence_is_not_over_parenthesized() {
    let formatted = format_query("MATCH (n) RETURN 1 + 2 * 3", &FormatOptions::default()).unwrap();
    assert!(formatted.contains("1 + 2 * 3"));
}

#[test]
fn edge_direction_formatting_preserves_direction_semantics() {
    let cases = [
        ("<-[e:ROAD]-(b)", "<-[e:ROAD]-(b)"),
        ("~[e:ROAD]~(b)", "~[e:ROAD]~(b)"),
        ("-[e:ROAD]->(b)", "-[e:ROAD]->(b)"),
        ("<~[e:ROAD]~(b)", "<~[e:ROAD]~(b)"),
        ("~[e:ROAD]~>(b)", "~[e:ROAD]~>(b)"),
        ("<-[e:ROAD]->(b)", "<-[e:ROAD]->(b)"),
        ("-[e:ROAD]-(b)", "-[e:ROAD]-(b)"),
        ("<~[e:ROAD]~>(b)", "-[e:ROAD]-(b)"),
    ];
    for (input, expected_pattern) in cases {
        let formatted = format_query(
            &format!("MATCH (a){input} RETURN a"),
            &FormatOptions::default(),
        )
        .unwrap();
        assert_eq!(
            formatted,
            format!("MATCH (a){expected_pattern}\nRETURN\n  a")
        );
        assert_eq!(
            format_query(&formatted, &FormatOptions::default()).unwrap(),
            formatted
        );
    }
}

#[test]
fn simplified_edge_direction_formatting_preserves_direction_semantics() {
    let cases = [
        ("-/ROAD/->", "-/ROAD/->"),
        ("<-/ROAD/-", "<-/ROAD/-"),
        ("~/ROAD/~", "~/ROAD/~"),
        ("<~/ROAD/~", "<~/ROAD/~"),
        ("~/ROAD/~>", "~/ROAD/~>"),
        ("<-/ROAD/->", "<-/ROAD/->"),
        ("-/ROAD/-", "-/ROAD/-"),
        ("<~/ROAD/~>", "-/ROAD/-"),
    ];
    for (input, expected_pattern) in cases {
        let formatted = format_query(
            &format!("MATCH (a){input}(b) RETURN a"),
            &FormatOptions::default(),
        )
        .unwrap();
        assert_eq!(
            formatted,
            format!("MATCH (a){expected_pattern}(b)\nRETURN\n  a")
        );
        assert_eq!(
            format_query(&formatted, &FormatOptions::default()).unwrap(),
            formatted
        );
    }
}

#[cfg(feature = "gleaph")]
#[test]
fn formats_social_demo_queries_and_preserves_nested_search_limit() {
    let queries = [
        "MATCH (feed:Feed {demo_id: 40})<-[e:IN_PUBLIC_FEED]-(p:Post)<-[:POSTED]-(author:User) OPTIONAL MATCH (p)-[:REPLY_TO]->(parent:Post) RETURN p.demo_id AS post_id, parent.demo_id AS parent_post_id, author.name AS author_name, p.body AS body, p.created_at AS created_at ORDER BY INSERTION(e) DESC LIMIT 20",
        "MATCH (u:User {demo_id: 1})<-[e:IN_HOME_FEED]-(p:Post)<-[:POSTED]-(author:User) WHERE p.is_public = TRUE OPTIONAL MATCH (p)-[:REPLY_TO]->(parent:Post) RETURN p.demo_id AS post_id, parent.demo_id AS parent_post_id, author.name AS author_name, p.body AS body, p.created_at AS created_at ORDER BY INSERTION(e) DESC LIMIT 20",
        "MATCH (p:Post)-[has_topic:HAS_TOPIC]->(t:Topic) WHERE t.demo_id = 13 MATCH (u:User)-[follows:FOLLOWS]->(author:User)-[posted:POSTED]->(p) WHERE u.demo_id = 1 RETURN p.demo_id AS post_id, author.name AS author_name, t.demo_id AS topic_id, p.body AS body, p.created_at AS created_at, ELEMENT_ID(follows) AS follows_edge_id, ELEMENT_ID(posted) AS posted_edge_id, ELEMENT_ID(has_topic) AS topic_edge_id",
        "MATCH (p:Post)<-[:POSTED]-(author:User) WHERE p.is_public = TRUE SEARCH p IN (VECTOR INDEX post_vec FOR $query LIMIT 10) DISTANCE AS distance RETURN p.demo_id AS post_id, author.name AS author_name, p.body AS body, distance ORDER BY distance ASC",
        "MATCH (u:User)-[:FOLLOWS]->(author:User)-[:POSTED]->(p:Post) WHERE u.demo_id = 1 AND p.is_public = TRUE SEARCH p IN (VECTOR INDEX post_vec FOR $query LIMIT 10) DISTANCE AS distance RETURN p.demo_id AS post_id, author.name AS author_name, p.body AS body, distance ORDER BY distance ASC",
    ];
    let options = FormatOptions::default();
    for query in queries {
        let formatted = format_query(query, &options).unwrap();
        assert!(formatted.contains("RETURN\n"));
        assert_eq!(format_query(&formatted, &options).unwrap(), formatted);
    }
    let search = format_query(queries[3], &options).unwrap();
    assert!(search.contains("VECTOR INDEX post_vec\n"));
    assert!(search.contains("LIMIT 10\n") || search.contains("LIMIT 10"));
    assert!(search.contains("\n) DISTANCE AS distance"));
}

#[cfg(not(feature = "gleaph"))]
#[test]
fn standard_feature_does_not_enable_gleaph_search() {
    let error = format_query(
        "MATCH (p:Post) SEARCH p IN (VECTOR INDEX post_vec FOR $query LIMIT 10) DISTANCE AS distance RETURN p",
        &FormatOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(error, FormatError::Parse(_)));
}

#[cfg(feature = "gleaph")]
#[test]
fn formats_grant_and_revoke_round_trip_with_canonical_nodes_keyword() {
    let options = FormatOptions::default();
    // VERTICES canonicalizes to NODES; the formatted form is stable under reformatting.
    let grant = format_query(
        "GRANT TRAVERSE OUTGOING ON GRAPH social VERTICES Person TO PRINCIPAL 'w7x7r-cok77-xa'",
        &options,
    )
    .unwrap();
    assert_eq!(
        grant,
        "GRANT TRAVERSE OUTGOING ON GRAPH social NODES Person TO PRINCIPAL 'w7x7r-cok77-xa'"
    );
    assert_eq!(format_query(&grant, &options).unwrap(), grant);

    let revoke = format_query(
        "REVOKE READ ON GRAPH social EDGES KNOWS FROM PUBLIC",
        &options,
    )
    .unwrap();
    assert_eq!(
        revoke,
        "REVOKE READ ON GRAPH social EDGES KNOWS FROM PUBLIC"
    );
}

#[cfg(feature = "gleaph")]
#[test]
fn formats_explain_authorization_round_trip() {
    let options = FormatOptions::default();
    let self_mode = format_query(
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY share_feed",
        &options,
    )
    .unwrap();
    assert_eq!(
        self_mode,
        "EXPLAIN AUTHORIZATION\nFOR PREPARED QUERY share_feed"
    );
    assert_eq!(format_query(&self_mode, &options).unwrap(), self_mode);

    let owner_mode = format_query(
        "EXPLAIN AUTHORIZATION FOR PREPARED QUERY share_feed BY PRINCIPAL 'w7x7r-cok77-xa'",
        &options,
    )
    .unwrap();
    assert_eq!(
        owner_mode,
        "EXPLAIN AUTHORIZATION\nFOR PREPARED QUERY share_feed\nBY PRINCIPAL 'w7x7r-cok77-xa'"
    );
    assert_eq!(format_query(&owner_mode, &options).unwrap(), owner_mode);
}

#[cfg(feature = "gleaph")]
#[test]
fn formats_conditional_policy_selector_round_trips_canonically() {
    let options = FormatOptions::default();
    let grant = format_query(
        "GRANT READ ON GRAPH social NODES Post FOR (p:Post) WHERE p.visibility = 'public' \
         AND p.owner = MSG_CALLER() AND p.stars >= 3 TO PUBLIC",
        &options,
    )
    .unwrap();
    assert_eq!(
        grant,
        "GRANT READ ON GRAPH social NODES Post FOR (p:Post) WHERE p.visibility = 'public' \
         AND p.owner = MSG_CALLER() AND p.stars >= 3 TO PUBLIC"
    );
    assert_eq!(format_query(&grant, &options).unwrap(), grant);

    // Edge selectors render in undirected canonical form regardless of input spelling.
    let edge_out = format_query(
        "GRANT MATCH ON GRAPH g NODES T FOR ()-[e:KNOWS]->() WHERE e.weight > 5 TO PUBLIC",
        &options,
    )
    .unwrap();
    assert_eq!(
        edge_out,
        "GRANT MATCH ON GRAPH g NODES T FOR ()-[e:KNOWS]-() WHERE e.weight > 5 TO PUBLIC"
    );
    let edge_in = format_query(
        "GRANT MATCH ON GRAPH g NODES T FOR <-[e:KNOWS]-() WHERE e.weight > 5 TO PUBLIC",
        &options,
    )
    .unwrap();
    assert_eq!(edge_in, edge_out);
}

#[cfg(feature = "gleaph")]
#[test]
fn formats_prepared_query_publication_round_trip() {
    let options = FormatOptions::default();
    let grant = format_query(
        "GRANT EXECUTE ON PREPARED QUERY find-users TO PUBLIC",
        &options,
    )
    .unwrap();
    assert_eq!(
        grant,
        "GRANT EXECUTE ON PREPARED QUERY find-users TO PUBLIC"
    );
    assert_eq!(format_query(&grant, &options).unwrap(), grant);

    let revoke = format_query(
        "REVOKE   EXECUTE   ON   PREPARED QUERY  find-users  FROM PUBLIC",
        &options,
    )
    .unwrap();
    assert_eq!(
        revoke,
        "REVOKE EXECUTE ON PREPARED QUERY find-users FROM PUBLIC"
    );
    assert_eq!(format_query(&revoke, &options).unwrap(), revoke);
}

#[cfg(not(feature = "gleaph"))]
#[test]
fn standard_feature_does_not_enable_gleaph_grant() {
    let error = format_query(
        "GRANT MATCH ON GRAPH g NODES Person TO PUBLIC",
        &FormatOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(error, FormatError::Parse(_)));
}

// ── Expression coverage: round-trip and precedence ──────────────────────
//
// These assert the rendered text, not only stability: a precedence mistake can be stable under a
// second format pass while re-parsing into a different expression.

fn round_trip(input: &str) -> String {
    let options = FormatOptions::default();
    let formatted = format_query(input, &options).unwrap_or_else(|e| panic!("{input}: {e}"));
    assert_eq!(
        format_query(&formatted, &options).unwrap_or_else(|e| panic!("{formatted}: {e}")),
        formatted,
        "formatting is not stable for {input}"
    );
    formatted
}

#[test]
fn precedence_is_preserved_in_boolean_expressions() {
    // `NOT` binds tighter than `AND`, which binds tighter than `OR`.
    assert_eq!(
        round_trip("MATCH (n) WHERE NOT (n.a = 1 OR n.b = 2) RETURN n"),
        "MATCH (n) WHERE NOT (n.a = 1 OR n.b = 2)\nRETURN\n  n"
    );
    assert_eq!(
        round_trip("MATCH (n) WHERE n.a = 1 AND n.b = 2 OR n.c = 3 RETURN n"),
        "MATCH (n) WHERE n.a = 1 AND n.b = 2 OR n.c = 3\nRETURN\n  n"
    );
}

#[test]
fn precedence_is_preserved_in_arithmetic_and_comparison() {
    assert_eq!(
        round_trip("MATCH (n) WHERE n.a + 1 > n.b * 2 RETURN n"),
        "MATCH (n) WHERE n.a + 1 > n.b * 2\nRETURN\n  n"
    );
    assert_eq!(
        round_trip("MATCH (n) WHERE (n.a + 1) * 2 = 4 RETURN n"),
        "MATCH (n) WHERE (n.a + 1) * 2 = 4\nRETURN\n  n"
    );
}

#[test]
fn aggregates_render_with_distinct_and_second_argument() {
    assert_eq!(
        round_trip("MATCH (n) RETURN COUNT(DISTINCT n.x)"),
        "MATCH (n)\nRETURN\n  COUNT(DISTINCT n.x)"
    );
    assert_eq!(
        round_trip("MATCH (n) RETURN PERCENTILE_DISC(n.x, 0.9)"),
        "MATCH (n)\nRETURN\n  PERCENTILE_DISC(n.x, 0.9)"
    );
}

#[test]
fn case_expressions_render_both_forms() {
    assert_eq!(
        round_trip("MATCH (n) RETURN CASE n.x WHEN 1 THEN 'a' ELSE 'b' END"),
        "MATCH (n)\nRETURN\n  CASE n.x WHEN 1 THEN 'a' ELSE 'b' END"
    );
    assert_eq!(
        round_trip("MATCH (n) RETURN CASE WHEN n.x = 1 THEN 'a' END"),
        "MATCH (n)\nRETURN\n  CASE WHEN n.x = 1 THEN 'a' END"
    );
}

#[test]
fn graph_predicates_render_with_negation() {
    assert_eq!(
        round_trip("MATCH (n)-[e]->(m) WHERE n IS NOT SOURCE OF e RETURN n"),
        "MATCH (n)-[e]->(m) WHERE n IS NOT SOURCE OF e\nRETURN\n  n"
    );
    assert_eq!(
        round_trip("MATCH (n) WHERE n.x IS NOT NFC NORMALIZED RETURN n"),
        "MATCH (n) WHERE n.x IS NOT NFC NORMALIZED\nRETURN\n  n"
    );
}

#[test]
fn constructed_values_render() {
    assert_eq!(round_trip("RETURN [1, 2]"), "RETURN\n  [1, 2]");
    assert_eq!(round_trip("RETURN {a: 1}"), "RETURN\n  {a: 1}");
    assert_eq!(
        round_trip("MATCH (a)-[e]->(b) RETURN PATH [a, e, b]"),
        "MATCH (a)-[e]->(b)\nRETURN\n  PATH [a, e, b]"
    );
}

#[test]
fn cast_and_value_type_predicate_render_types() {
    // CAST renders `AS`, not a bare type.
    assert_eq!(
        round_trip("MATCH (n) RETURN CAST(n.x AS DECIMAL(10, 2))"),
        "MATCH (n)\nRETURN\n  CAST(n.x AS DECIMAL(10, 2))"
    );
    // The `::` spelling canonicalizes to `TYPED`, which parses to the same predicate.
    assert_eq!(
        round_trip("MATCH (n) WHERE n.x IS :: INT64 RETURN n"),
        "MATCH (n) WHERE n.x IS TYPED INT64\nRETURN\n  n"
    );
    assert_eq!(
        round_trip("MATCH (n) WHERE n.x IS NOT TYPED STRING(8) RETURN n"),
        "MATCH (n) WHERE n.x IS NOT TYPED STRING(8)\nRETURN\n  n"
    );
}

// ── Function and subquery coverage ──────────────────────────────────────

#[test]
fn scalar_and_string_functions_render() {
    assert_eq!(round_trip("RETURN UPPER('a')"), "RETURN\n  UPPER('a')");
    assert_eq!(round_trip("RETURN ABS(-1)"), "RETURN\n  ABS(-1)");
    assert_eq!(
        round_trip("RETURN CHAR_LENGTH('abc')"),
        "RETURN\n  CHAR_LENGTH('abc')"
    );
    assert_eq!(round_trip("RETURN BTRIM(' a ')"), "RETURN\n  BTRIM(' a ')");
    assert_eq!(
        round_trip("RETURN TRIM(LEADING 'x' FROM 'xax')"),
        "RETURN\n  TRIM(LEADING 'x' FROM 'xax')"
    );
    assert_eq!(
        round_trip("RETURN TRIM(LEADING FROM '  a')"),
        "RETURN\n  TRIM(LEADING FROM '  a')"
    );
    assert_eq!(round_trip("RETURN MOD(5, 2)"), "RETURN\n  MOD(5, 2)");
}

#[test]
fn temporal_literals_and_functions_render() {
    assert_eq!(
        round_trip("RETURN DATE '2024-01-01'"),
        "RETURN\n  DATE '2024-01-01'"
    );
    assert_eq!(
        round_trip("RETURN DURATION 'P1D'"),
        "RETURN\n  DURATION 'P1D'"
    );
    assert_eq!(round_trip("RETURN CURRENT_DATE"), "RETURN\n  CURRENT_DATE");
    assert_eq!(round_trip("RETURN SESSION_USER"), "RETURN\n  SESSION_USER");
}

#[test]
fn exists_subquery_renders() {
    assert_eq!(
        round_trip("MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n"),
        "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) }\nRETURN\n  n"
    );
}

#[test]
fn value_subquery_renders() {
    assert_eq!(
        round_trip("RETURN VALUE { MATCH (n) RETURN n }"),
        "RETURN\n  VALUE { MATCH (n)\nRETURN\n  n }"
    );
}

// ── Statement-level coverage ───────────────────────────────────────────

#[test]
fn data_modifying_statements_render() {
    for expected in [
        "INSERT (:A)",
        "INSERT (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person)",
        "SET n.x = 1",
        "REMOVE n:Label",
        "DELETE n",
        "DETACH DELETE n",
        "NODETACH DELETE n",
    ] {
        assert_eq!(round_trip(expected), expected);
    }
    // A linear query carries DML parts with the same renderer as a top-level statement.
    assert_eq!(
        round_trip("MATCH (n) SET n.x = 1 REMOVE n:Old DELETE m RETURN n"),
        "MATCH (n)\nSET n.x = 1\nREMOVE n:Old\nDELETE m\nRETURN\n  n"
    );
}

#[test]
fn catalog_statements_render() {
    for expected in [
        "CREATE SCHEMA /a",
        "CREATE SCHEMA /a/b",
        "DROP SCHEMA /a",
        "CREATE GRAPH g ANY",
        "CREATE PROPERTY GRAPH g ANY",
        "CREATE GRAPH g { NODE Person }",
        "CREATE GRAPH g AS COPY OF h",
        "DROP GRAPH g",
        "DROP PROPERTY GRAPH g",
        "CREATE GRAPH TYPE t AS { NODE Person }",
        "CREATE PROPERTY GRAPH TYPE t { NODE Person }",
        "DROP GRAPH TYPE t",
    ] {
        assert_eq!(round_trip(expected), expected);
    }
}

#[test]
fn graph_type_elements_keep_their_pattern_shape() {
    // The pattern form has no type name, so it cannot be rendered as the phrase form.
    assert_eq!(
        round_trip("CREATE GRAPH TYPE t { (:A&B) }"),
        "CREATE GRAPH TYPE t { (:A&B) }"
    );
    assert_eq!(
        round_trip("CREATE GRAPH TYPE t { (n:Person)-[:R]->(m:Person) }"),
        "CREATE GRAPH TYPE t { (n:Person)-[:R]->(m:Person) }"
    );
    assert_eq!(
        round_trip("CREATE GRAPH TYPE t { NODE Person LABEL Person {age INT64} }"),
        "CREATE GRAPH TYPE t { NODE Person LABEL Person {age INT64} }"
    );
}

#[test]
fn session_and_transaction_envelope_renders() {
    for expected in [
        "SESSION SET GRAPH g",
        "SESSION SET PROPERTY GRAPH g",
        "SESSION SET SCHEMA CURRENT_SCHEMA",
        "SESSION SET VALUE $x = 1",
        "SESSION RESET",
        "SESSION RESET SCHEMA",
        "SESSION CLOSE",
        "START TRANSACTION READ WRITE",
        "COMMIT",
        "ROLLBACK",
    ] {
        assert_eq!(round_trip(expected), expected);
    }
    assert_eq!(
        round_trip("START TRANSACTION READ ONLY MATCH (n) RETURN n COMMIT"),
        "START TRANSACTION READ ONLY\nMATCH (n)\nRETURN\n  n\nCOMMIT"
    );
    assert_eq!(
        round_trip("MATCH (n) RETURN n NEXT YIELD n MATCH (n) RETURN n"),
        "MATCH (n)\nRETURN\n  n\nNEXT YIELD n MATCH (n)\nRETURN\n  n"
    );
}

// ── Procedure calls and MATCH YIELD ────────────────────────────────────

#[test]
fn procedure_calls_render() {
    assert_eq!(round_trip("CALL db.labels()"), "CALL db.labels");
    assert_eq!(
        round_trip("CALL db.labels() YIELD lbl RETURN lbl"),
        "CALL db.labels YIELD lbl\nRETURN\n  lbl"
    );
    assert_eq!(
        round_trip("OPTIONAL CALL p( 1, 2 )"),
        "OPTIONAL CALL p(1, 2)"
    );
    assert_eq!(
        round_trip("CALL GLEAPH.FINALIZE_BULK_INGEST()"),
        "CALL GLEAPH.FINALIZE_BULK_INGEST"
    );
}

#[test]
fn inline_procedure_calls_render() {
    assert_eq!(
        round_trip("CALL { MATCH (n) RETURN n }"),
        "CALL { MATCH (n)\nRETURN\n  n }"
    );
    assert_eq!(
        round_trip("CALL (a, b) { MATCH (n) RETURN n }"),
        "CALL (a, b) { MATCH (n)\nRETURN\n  n }"
    );
    // The `USE <graph> { ... }` body is an inline procedure call in the AST but has no CALL.
    assert_eq!(
        round_trip("USE g { MATCH (n) RETURN n }"),
        "USE g { MATCH (n)\nRETURN\n  n }"
    );
}

#[test]
fn match_yield_renders() {
    assert_eq!(
        round_trip("MATCH (n) YIELD n RETURN n"),
        "MATCH (n) YIELD n\nRETURN\n  n"
    );
    // 16.3: the graph-pattern form has no alias, and `NO BINDINGS` is a valid item list.
    assert_eq!(
        round_trip("MATCH (n) YIELD NO BINDINGS RETURN 1"),
        "MATCH (n) YIELD NO BINDINGS\nRETURN\n  1"
    );
}

/// 16.14's `<yield clause>` (used by `CALL`/`NEXT`) is the one that takes an alias.
#[test]
fn call_and_next_yield_keep_their_alias() {
    assert_eq!(
        round_trip("CALL myproc() YIELD x AS y RETURN y"),
        "CALL myproc YIELD x AS y\nRETURN\n  y"
    );
    assert_eq!(
        round_trip("MATCH (n) NEXT YIELD n AS m RETURN m"),
        "MATCH (n)\nNEXT YIELD n AS m RETURN\n  m"
    );
}

// ── Linear-query prefix and FOR ordinality ─────────────────────────────

#[test]
fn linear_query_prefix_renders() {
    assert_eq!(
        round_trip("AT /a MATCH (n) RETURN n"),
        "AT /a\nMATCH (n)\nRETURN\n  n"
    );
    assert_eq!(
        round_trip("VALUE x = 1 MATCH (n) RETURN n"),
        "VALUE x = 1\nMATCH (n)\nRETURN\n  n"
    );
    assert_eq!(
        round_trip("GRAPH g = h MATCH (n) RETURN n"),
        "GRAPH g = h\nMATCH (n)\nRETURN\n  n"
    );
    // The table initializer's `{ ... }` body is a composite query, not an expression.
    assert_eq!(
        round_trip("TABLE t = { MATCH (n) RETURN n } MATCH (m) RETURN m"),
        "TABLE t = { MATCH (n)\nRETURN\n  n }\nMATCH (m)\nRETURN\n  m"
    );
}

#[test]
fn for_ordinality_renders_both_keywords() {
    assert_eq!(
        round_trip("MATCH (n) FOR x IN [1, 2] WITH ORDINALITY i RETURN x"),
        "MATCH (n)\nFOR x IN [1, 2] WITH ORDINALITY i\nRETURN\n  x"
    );
    assert_eq!(
        round_trip("MATCH (n) FOR x IN [1, 2] WITH OFFSET i RETURN x"),
        "MATCH (n)\nFOR x IN [1, 2] WITH OFFSET i\nRETURN\n  x"
    );
}

// ── Graph pattern modifiers ────────────────────────────────────────────

#[test]
fn match_modes_render_with_their_recorded_spelling() {
    for expected in [
        "MATCH REPEATABLE ELEMENTS (a)-[e]->(b) RETURN a",
        "MATCH REPEATABLE ELEMENT BINDINGS (a)-[e]->(b) RETURN a",
        "MATCH DIFFERENT EDGES (a)-[e]->(b) RETURN a",
        "MATCH DIFFERENT RELATIONSHIP BINDINGS (a)-[e]->(b) RETURN a",
    ] {
        assert_eq!(
            round_trip(expected),
            expected.replace(" RETURN a", "\nRETURN\n  a")
        );
    }
}

#[test]
fn path_prefixes_render_all_search_forms() {
    for expected in [
        "MATCH WALK (a)-[e]->(b) RETURN a",
        "MATCH TRAIL PATHS (a)-[e]->(b) RETURN a",
        "MATCH ALL PATHS (a)-[e]->(b) RETURN a",
        "MATCH ALL SHORTEST (a)-[e]->{1,2}(b) RETURN a",
        "MATCH ANY SHORTEST (a)-[e]->{1,2}(b) RETURN a",
        "MATCH ANY 3 PATHS (a)-[e]->(b) RETURN a",
        "MATCH ANY $k PATHS (a)-[e]->(b) RETURN a",
        "MATCH SHORTEST 2 (a)-[e]->{1,2}(b) RETURN a",
        "MATCH SHORTEST 2 PATHS GROUP (a)-[e]->(b) RETURN a",
        "MATCH SHORTEST 2 GROUPS (a)-[e]->(b) RETURN a",
        "MATCH ANY SHORTEST WALK (a)-[e]->{1,2}(b) RETURN a",
    ] {
        assert_eq!(
            round_trip(expected),
            expected.replace(" RETURN a", "\nRETURN\n  a")
        );
    }
    // A path variable and a vendor extension attach to the same path.
    assert_eq!(
        round_trip("MATCH p = ANY SHORTEST (a)-[e]->{1,2}(b) RETURN p"),
        "MATCH p = ANY SHORTEST (a)-[e]->{1,2}(b)\nRETURN\n  p"
    );
    assert_eq!(
        round_trip("MATCH ANY SHORTEST (a)-[e:L]->{1,5}(b) VENDOR_COST BY f(e) RETURN a"),
        "MATCH ANY SHORTEST (a)-[e:L]->{1,5}(b) VENDOR_COST BY f(e)\nRETURN\n  a"
    );
}

#[test]
fn path_alternations_keep_their_distinct_spellings() {
    // `|` and `|+|` are different operators; rendering one as the other would be stable under a
    // second format pass and still change the query.
    assert_eq!(
        round_trip("MATCH (a)-[e]->(b) | (a)-[f]->(c) RETURN a"),
        "MATCH (a)-[e]->(b) | (a)-[f]->(c)\nRETURN\n  a"
    );
    assert_eq!(
        round_trip("MATCH (a)-[e]->(b) |+| (a)-[f]->(c) RETURN a"),
        "MATCH (a)-[e]->(b) |+| (a)-[f]->(c)\nRETURN\n  a"
    );
}

#[test]
fn keep_and_parenthesized_subpaths_render() {
    assert_eq!(
        round_trip("MATCH (a)-[e]->(b) KEEP TRAIL RETURN a"),
        "MATCH (a)-[e]->(b) KEEP TRAIL\nRETURN\n  a"
    );
    assert_eq!(
        round_trip("MATCH ((a)-[e]->(b) WHERE b.x = 1) RETURN a"),
        "MATCH ((a)-[e]->(b) WHERE b.x = 1)\nRETURN\n  a"
    );
    assert_eq!(
        round_trip("MATCH (a)((u)-[e:L]->(v)){1,2}(c) RETURN c"),
        "MATCH (a)((u)-[e:L]->(v)){1,2}(c)\nRETURN\n  c"
    );
}

// ── Precedence ladder, index/slice and bounded types ───────────────────

#[test]
fn concatenation_binds_between_comparison_and_addition() {
    // The parser's ladder is Or < And < Not < Is < Concat < Add < Mul: `||` is looser than `+`
    // and tighter than a comparison, so neither direction may lose its parentheses.
    assert_eq!(round_trip("RETURN 'a' || 'b'"), "RETURN\n  'a' || 'b'");
    assert_eq!(
        round_trip("RETURN 'a' || ('b' + 'c')"),
        "RETURN\n  'a' || ('b' + 'c')"
    );
    assert_eq!(
        round_trip("MATCH (n) WHERE n.x || 'a' = 'b' RETURN n"),
        "MATCH (n) WHERE n.x || 'a' = 'b'\nRETURN\n  n"
    );
}

#[cfg(feature = "cypher")]
#[test]
fn list_index_and_slice_render() {
    assert_eq!(round_trip("RETURN [1, 2][0]"), "RETURN\n  [1, 2][0]");
    assert_eq!(round_trip("RETURN [1, 2][1..]"), "RETURN\n  [1, 2][1..]");
    assert_eq!(round_trip("RETURN [1, 2][..1]"), "RETURN\n  [1, 2][..1]");
    assert_eq!(
        round_trip("RETURN [1, 2, 3][1..2]"),
        "RETURN\n  [1, 2, 3][1..2]"
    );
}

#[test]
fn length_bounded_types_render() {
    assert_eq!(
        round_trip("RETURN CAST('a' AS STRING(1, 5))"),
        "RETURN\n  CAST('a' AS STRING(1, 5))"
    );
    assert_eq!(
        round_trip("RETURN CAST('a' AS STRING(5))"),
        "RETURN\n  CAST('a' AS STRING(5))"
    );
    assert_eq!(
        round_trip("RETURN CAST('a' AS VARCHAR(5))"),
        "RETURN\n  CAST('a' AS VARCHAR(5))"
    );
}

// ── SELECT with a FROM source ──────────────────────────────────────────

#[test]
fn select_from_graph_match_renders() {
    assert_eq!(
        round_trip("SELECT n.x FROM g MATCH (n)"),
        "SELECT\n  n.x\nFROM g MATCH (n)"
    );
    assert_eq!(
        round_trip(
            "SELECT DISTINCT COUNT(*) FROM g MATCH (n) WHERE n.x > 1 GROUP BY n.y ORDER BY n.y LIMIT 3"
        ),
        "SELECT DISTINCT\n  COUNT(*)\nFROM g MATCH (n) WHERE n.x > 1\nGROUP BY n.y\nORDER BY n.y\nLIMIT 3"
    );
    assert_eq!(
        round_trip("SELECT * FROM g1 MATCH (n), g2 MATCH (m)"),
        "SELECT *\nFROM g1 MATCH (n), g2 MATCH (m)"
    );
}

#[test]
fn select_from_query_specification_renders() {
    assert_eq!(
        round_trip("SELECT * FROM { MATCH (n) RETURN n }"),
        "SELECT *\nFROM { MATCH (n)\nRETURN\n  n }"
    );
}

#[test]
fn select_without_source_is_unchanged() {
    assert_eq!(round_trip("SELECT 1"), "SELECT\n  1");
}

// ── SELECT-level WHERE ─────────────────────────────────────────────────

#[test]
fn select_level_where_renders_after_the_source() {
    // A query-specification source cannot consume the WHERE, so it belongs to the select statement.
    assert_eq!(
        round_trip("SELECT * FROM { MATCH (n) RETURN n } WHERE true"),
        "SELECT *\nFROM { MATCH (n)\nRETURN\n  n }\nWHERE TRUE"
    );
    // A graph-match source keeps its own WHERE inline, which is the standard's lifting case.
    assert_eq!(
        round_trip("SELECT n.x FROM g MATCH (n) WHERE n.x > 1"),
        "SELECT\n  n.x\nFROM g MATCH (n) WHERE n.x > 1"
    );
}

// ── FROM match modifiers ───────────────────────────────────────────────

#[test]
fn from_match_modifiers_round_trip() {
    // A `selectGraphMatch` is a full match statement, so a mode, a KEEP prefix and a YIELD render.
    for expected in [
        "SELECT *\nFROM g MATCH REPEATABLE ELEMENTS (n)",
        "SELECT *\nFROM g MATCH (n) KEEP WALK",
        "SELECT *\nFROM g MATCH (n) YIELD n",
        "SELECT *\nFROM g1 MATCH (a), (b), g2 MATCH DIFFERENT EDGES (m)",
    ] {
        assert_eq!(round_trip(expected), expected);
    }
}

#[test]
fn graph_type_sources_keep_their_spelling() {
    // 12.6 gives `LIKE`, `COPY OF` and the nested form different meanings; the formatter must not
    // rewrite one into another.
    assert_eq!(
        round_trip("CREATE GRAPH TYPE t LIKE g"),
        "CREATE GRAPH TYPE t LIKE g"
    );
    assert_eq!(
        round_trip("CREATE GRAPH TYPE t COPY OF g"),
        "CREATE GRAPH TYPE t COPY OF g"
    );
    assert_eq!(
        round_trip("CREATE GRAPH TYPE t AS COPY OF g"),
        "CREATE GRAPH TYPE t AS COPY OF g"
    );
    assert_eq!(
        round_trip("CREATE GRAPH TYPE t AS {}"),
        "CREATE GRAPH TYPE t AS {}"
    );
}
