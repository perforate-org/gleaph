//! Additional pattern coverage tests — targeting uncovered lines in parser/pattern.rs.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::EdgeDirection;

/// Extract the graph pattern from the first match statement.
fn graph_pat(prog: &GqlProgram) -> &GraphPattern {
    let b = body(prog);
    match &b.first {
        Statement::Query(cq) => {
            for part in &cq.left.parts {
                if let SimpleQueryStatement::Match(m) = part {
                    return &m.pattern;
                }
            }
            panic!("no Match found");
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

/// Extract the first edge direction from a match pattern.
fn first_edge_dir(input: &str) -> EdgeDirection {
    let prog = p(input);
    let gp = graph_pat(&prog);
    let path = &gp.paths[0];
    match &path.expr {
        PathPatternExpr::Term(term) => {
            for f in &term.factors {
                if let PathPrimary::Edge(e) = &f.primary {
                    return e.direction.clone();
                }
            }
            panic!("no edge found");
        }
        _ => panic!("expected Term"),
    }
}

// ── Match modes ─────────────────────────────────────────────────────────

mod match_modes {
    use super::*;

    #[test]
    fn repeatable_elements() {
        let prog = p("MATCH REPEATABLE ELEMENTS (n)-[e]->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(matches!(
            gp.match_mode,
            Some(MatchMode::RepeatableElements { .. })
        ));
    }

    #[test]
    fn different_relationship() {
        let prog = p("MATCH DIFFERENT RELATIONSHIP (n)-[e]->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(matches!(
            gp.match_mode,
            Some(MatchMode::DifferentEdges { .. })
        ));
    }

    #[test]
    fn different_edges() {
        let prog = p("MATCH DIFFERENT EDGES (n)-[e]->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(matches!(
            gp.match_mode,
            Some(MatchMode::DifferentEdges { .. })
        ));
    }

    #[test]
    fn different_relationships() {
        let prog = p("MATCH DIFFERENT RELATIONSHIPS (n)-[e]->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(matches!(
            gp.match_mode,
            Some(MatchMode::DifferentEdges { .. })
        ));
    }

    #[test]
    fn different_edge_bindings() {
        let prog = p("MATCH DIFFERENT EDGE BINDINGS (n)-[e]->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(matches!(
            gp.match_mode,
            Some(MatchMode::DifferentEdges { .. })
        ));
    }

    #[test]
    fn different_relationship_bindings() {
        let prog = p("MATCH DIFFERENT RELATIONSHIP BINDINGS (n)-[e]->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(matches!(
            gp.match_mode,
            Some(MatchMode::DifferentEdges { .. })
        ));
    }
}

// ── Edge directions (full bracket patterns) ─────────────────────────────

mod edge_directions {
    use super::*;

    #[test]
    fn left_edge() {
        let dir = first_edge_dir("MATCH (n)<-[e]-(m) RETURN n");
        assert_eq!(dir, EdgeDirection::PointingLeft);
    }

    #[test]
    fn any_direction_edge() {
        // -[e]- is "any direction"
        let prog = p("MATCH (n)-[e]-(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(!gp.paths.is_empty());
    }

    #[test]
    fn left_or_right_edge() {
        let prog = p("MATCH (n)<-[e]->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(!gp.paths.is_empty());
    }

    #[test]
    fn undirected_edge() {
        let prog = p("MATCH (n)~[e]~(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(!gp.paths.is_empty());
    }

    #[test]
    fn undirected_or_right_edge() {
        let prog = p("MATCH (n)~[e]~>(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(!gp.paths.is_empty());
    }

    #[test]
    fn left_or_undirected_edge() {
        let prog = p("MATCH (n)<~[e]~(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(!gp.paths.is_empty());
    }
}

// ── Parenthesized label expression ──────────────────────────────────────

mod label_expr_tests {
    use super::*;

    #[test]
    fn parenthesized_label() {
        let prog = p("MATCH (n :(Person|Employee)) RETURN n");
        let gp = graph_pat(&prog);
        assert!(!gp.paths.is_empty());
    }
}

// ── Insert edge with IS keyword ─────────────────────────────────────────

mod insert_edge {
    use super::*;

    #[test]
    fn insert_edge_with_is() {
        let prog = p("INSERT (a)-[e IS Friend]->(b)");
        let b = body(&prog);
        match &b.first {
            Statement::Insert(ins) => {
                assert!(!ins.patterns.is_empty());
                // Should have node-edge-node (3 elements)
                assert_eq!(ins.patterns[0].elements.len(), 3);
                if let InsertElement::Edge(e) = &ins.patterns[0].elements[1] {
                    assert_eq!(e.is_or_colon, Some(IsOrColon::Is));
                } else {
                    panic!("expected Edge");
                }
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }

    #[test]
    fn insert_edge_left() {
        let prog = p("INSERT (a)<-[e :Friend]-(b)");
        let b = body(&prog);
        match &b.first {
            Statement::Insert(ins) => {
                if let InsertElement::Edge(e) = &ins.patterns[0].elements[1] {
                    assert_eq!(e.direction, EdgeDirection::PointingLeft);
                } else {
                    panic!("expected Edge");
                }
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }

    #[test]
    fn insert_edge_undirected() {
        let prog = p("INSERT (a)~[e :Friend]~(b)");
        let b = body(&prog);
        match &b.first {
            Statement::Insert(ins) => {
                if let InsertElement::Edge(e) = &ins.patterns[0].elements[1] {
                    assert_eq!(e.direction, EdgeDirection::Undirected);
                } else {
                    panic!("expected Edge");
                }
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }
}

// ── Simplified path patterns ────────────────────────────────────────────

mod simplified_path {
    use super::*;

    #[test]
    fn basic_simplified() {
        let prog = p("MATCH (n)-/Friend/->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(!gp.paths.is_empty());
    }

    #[test]
    fn simplified_union() {
        let prog = p("MATCH (n)-/Friend|Colleague/->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(!gp.paths.is_empty());
    }

    #[test]
    fn simplified_conjunction() {
        let prog = p("MATCH (n)-/Friend&Active/->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(!gp.paths.is_empty());
    }

    #[test]
    fn simplified_negation() {
        let prog = p("MATCH (n)-/!Blocked/->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(!gp.paths.is_empty());
    }

    #[test]
    fn simplified_wildcard() {
        let prog = p("MATCH (n)-/%/->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(!gp.paths.is_empty());
    }

    #[test]
    fn simplified_quantified() {
        let prog = p("MATCH (n)-/Friend{1,3}/->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(!gp.paths.is_empty());
    }

    #[test]
    fn simplified_parenthesized() {
        let prog = p("MATCH (n)-/(Friend|Colleague)/->(m) RETURN n");
        let gp = graph_pat(&prog);
        assert!(!gp.paths.is_empty());
    }
}

// ── Property map in node pattern ────────────────────────────────────────

mod property_map {
    use super::*;

    #[test]
    fn node_with_properties() {
        let prog = p("MATCH (n {name: 'Alice', age: 30}) RETURN n");
        let gp = graph_pat(&prog);
        // Should parse with a single path containing a node with 2 properties
        assert!(!gp.paths.is_empty());
    }
}
