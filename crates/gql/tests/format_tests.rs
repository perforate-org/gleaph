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
    let error = format_query("MATCH (n) RETURN [n]", &FormatOptions::default()).unwrap_err();
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

#[cfg(feature = "gleaph")]
#[test]
fn formats_social_demo_queries_and_preserves_nested_search_limit() {
    let queries = [
        "MATCH (feed:Feed {demo_id: 40})<-[e:IN_PUBLIC_FEED]-(p:Post)<-[:POSTED]-(author:User) OPTIONAL MATCH (p)-[:REPLY_TO]->(parent:Post) RETURN p.demo_id AS post_id, parent.demo_id AS parent_post_id, author.name AS author_name, p.body AS body, p.created_at AS created_at ORDER BY GLEAPH.SEQUENCE(e) DESC LIMIT 20",
        "MATCH (u:User {demo_id: 1})<-[e:IN_HOME_FEED]-(p:Post)<-[:POSTED]-(author:User) WHERE p.is_public = TRUE OPTIONAL MATCH (p)-[:REPLY_TO]->(parent:Post) RETURN p.demo_id AS post_id, parent.demo_id AS parent_post_id, author.name AS author_name, p.body AS body, p.created_at AS created_at ORDER BY GLEAPH.SEQUENCE(e) DESC LIMIT 20",
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
    assert!(search.contains(") DISTANCE AS distance"));
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
