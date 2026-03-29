//! §16.4 — Graph pattern (match modes).
//!
//! GQL rule: `matchMode : REPEATABLE ELEMENTS | DIFFERENT EDGES`

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

/// Helper to extract the MatchStatement from a query string.
fn ms(input: &str) -> MatchStatement {
    let prog = p(input);
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            for part in &cq.left.parts {
                if let SimpleQueryStatement::Match(m) = part {
                    return m.clone();
                }
            }
            panic!("no Match found in parts: {:?}", cq.left.parts);
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

// ── matchMode ───────────────────────────────────────────────────────────
//   : REPEATABLE ELEMENTS | DIFFERENT EDGES
//   ;
mod match_mode {
    use super::*;

    /// MATCH REPEATABLE ELEMENTS (n)-[e]->(m) RETURN n
    /// → pattern.match_mode == Some(RepeatableElements)
    #[test]
    fn repeatable_elements() {
        let m = ms("MATCH REPEATABLE ELEMENTS (n)-[e]->(m) RETURN n");
        assert_eq!(
            m.pattern.match_mode,
            Some(MatchMode::RepeatableElements {
                keyword: MatchModeElementKeyword::Elements
            }),
            "expected RepeatableElements"
        );
    }

    /// MATCH DIFFERENT EDGES (n)-[e]->(m) RETURN n
    /// → pattern.match_mode == Some(DifferentEdges)
    #[test]
    fn different_edges() {
        let m = ms("MATCH DIFFERENT EDGES (n)-[e]->(m) RETURN n");
        assert_eq!(
            m.pattern.match_mode,
            Some(MatchMode::DifferentEdges {
                keyword: MatchModeEdgeKeyword::Edges
            }),
            "expected DifferentEdges"
        );
    }

    /// MATCH (n) RETURN n — no match mode
    /// → pattern.match_mode == None
    #[test]
    fn no_match_mode() {
        let m = ms("MATCH (n) RETURN n");
        assert_eq!(m.pattern.match_mode, None, "expected no match mode");
    }

    /// MATCH REPEATABLE ELEMENT (n)-[e]->(m) RETURN n
    #[test]
    fn repeatable_element_singular() {
        let m = ms("MATCH REPEATABLE ELEMENT (n)-[e]->(m) RETURN n");
        assert_eq!(
            m.pattern.match_mode,
            Some(MatchMode::RepeatableElements {
                keyword: MatchModeElementKeyword::Element
            }),
        );
    }

    /// MATCH REPEATABLE ELEMENT BINDINGS (n)-[e]->(m) RETURN n
    #[test]
    fn repeatable_element_bindings() {
        let m = ms("MATCH REPEATABLE ELEMENT BINDINGS (n)-[e]->(m) RETURN n");
        assert_eq!(
            m.pattern.match_mode,
            Some(MatchMode::RepeatableElements {
                keyword: MatchModeElementKeyword::ElementBindings
            }),
        );
    }

    /// MATCH DIFFERENT EDGE (n)-[e]->(m) RETURN n
    #[test]
    fn different_edge_singular() {
        let m = ms("MATCH DIFFERENT EDGE (n)-[e]->(m) RETURN n");
        assert_eq!(
            m.pattern.match_mode,
            Some(MatchMode::DifferentEdges {
                keyword: MatchModeEdgeKeyword::Edge
            }),
        );
    }

    /// MATCH DIFFERENT EDGE BINDINGS (n)-[e]->(m) RETURN n
    #[test]
    fn different_edge_bindings() {
        let m = ms("MATCH DIFFERENT EDGE BINDINGS (n)-[e]->(m) RETURN n");
        assert_eq!(
            m.pattern.match_mode,
            Some(MatchMode::DifferentEdges {
                keyword: MatchModeEdgeKeyword::EdgeBindings
            }),
        );
    }

    /// MATCH DIFFERENT RELATIONSHIP (n)-[e]->(m) RETURN n
    #[test]
    fn different_relationship() {
        let m = ms("MATCH DIFFERENT RELATIONSHIP (n)-[e]->(m) RETURN n");
        assert_eq!(
            m.pattern.match_mode,
            Some(MatchMode::DifferentEdges {
                keyword: MatchModeEdgeKeyword::Relationship
            }),
        );
    }

    /// MATCH DIFFERENT RELATIONSHIP BINDINGS (n)-[e]->(m) RETURN n
    #[test]
    fn different_relationship_bindings() {
        let m = ms("MATCH DIFFERENT RELATIONSHIP BINDINGS (n)-[e]->(m) RETURN n");
        assert_eq!(
            m.pattern.match_mode,
            Some(MatchMode::DifferentEdges {
                keyword: MatchModeEdgeKeyword::RelationshipBindings
            }),
        );
    }

    /// MATCH DIFFERENT RELATIONSHIPS (n)-[e]->(m) RETURN n
    #[test]
    fn different_relationships() {
        let m = ms("MATCH DIFFERENT RELATIONSHIPS (n)-[e]->(m) RETURN n");
        assert_eq!(
            m.pattern.match_mode,
            Some(MatchMode::DifferentEdges {
                keyword: MatchModeEdgeKeyword::Relationships
            }),
        );
    }
}

// ── keepClause ──────────────────────────────────────────────────────────
//   : KEEP pathPatternPrefix
//   ;
mod keep_clause {
    use super::*;

    /// MATCH (n)-[e]->(m) KEEP TRAIL RETURN n → keep is Some with Mode(Trail)
    #[test]
    fn keep_trail() {
        let m = ms("MATCH (n)-[e]->(m) KEEP TRAIL RETURN n");
        let keep = m.pattern.keep.as_ref().expect("expected keep clause");
        assert_eq!(
            keep.prefix,
            PathPatternPrefix::Mode {
                mode: PathMode::Trail,
                path_keyword: None
            },
            "expected KEEP TRAIL"
        );
    }

    /// MATCH (n)-[e]->(m) KEEP ALL SHORTEST RETURN n
    #[test]
    fn keep_all_shortest() {
        let m = ms("MATCH (n)-[e]->(m) KEEP ALL SHORTEST RETURN n");
        let keep = m.pattern.keep.as_ref().expect("expected keep clause");
        assert_eq!(
            keep.prefix,
            PathPatternPrefix::Search(SearchPrefix::AllShortest {
                mode: None,
                path_keyword: None
            }),
        );
    }

    /// MATCH (n)-[e]->(m) WHERE n.x > 1 RETURN n — graph pattern WHERE
    #[test]
    fn graph_pattern_where() {
        let m = ms("MATCH (n)-[e]->(m) WHERE n.x > 1 RETURN n");
        assert!(
            m.pattern.where_clause.is_some(),
            "expected WHERE clause on graph pattern"
        );
    }
}
