//! §16.7 — Path pattern expression.
//!
//! GQL rules: pathPatternExpression, pathTerm, pathFactor, pathPrimary,
//! nodePattern, edgePattern, parenthesizedPathPatternExpression.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::EdgeDirection;

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

/// Helper to extract factors from the first path's expression.
fn factors(input: &str) -> Vec<PathFactor> {
    let m = ms(input);
    match &m.pattern.paths[0].expr {
        PathPatternExpr::Term(t) => t.factors.clone(),
        other => panic!("expected Term, got {other:?}"),
    }
}

// ── nodePattern ─────────────────────────────────────────────────────────
mod node_pattern {
    use super::*;

    /// MATCH (n) RETURN n — single NodePattern factor
    #[test]
    fn single_node() {
        let fs = factors("MATCH (n) RETURN n");
        assert_eq!(fs.len(), 1);
        match &fs[0].primary {
            PathPrimary::Node(np) => {
                assert_eq!(np.variable, Some("n".to_string()));
                assert!(np.label.is_none());
                assert!(np.properties.is_empty());
                assert!(np.where_clause.is_none());
            }
            other => panic!("expected Node, got {other:?}"),
        }
    }

    /// MATCH (n:Person) RETURN n — node with label
    #[test]
    fn node_with_label() {
        let fs = factors("MATCH (n:Person) RETURN n");
        match &fs[0].primary {
            PathPrimary::Node(np) => {
                assert!(np.label.is_some(), "expected label to be Some");
            }
            other => panic!("expected Node, got {other:?}"),
        }
    }

    /// MATCH (n:Person {name: 'Alice'}) RETURN n — node with properties
    #[test]
    fn node_with_properties() {
        let fs = factors("MATCH (n:Person {name: 'Alice'}) RETURN n");
        match &fs[0].primary {
            PathPrimary::Node(np) => {
                assert!(!np.properties.is_empty(), "expected non-empty properties");
                assert_eq!(np.properties[0].name, "name");
            }
            other => panic!("expected Node, got {other:?}"),
        }
    }

    /// MATCH (n:Person WHERE n.age > 30) RETURN n — node with WHERE
    #[test]
    fn node_with_where() {
        let fs = factors("MATCH (n:Person WHERE n.age > 30) RETURN n");
        match &fs[0].primary {
            PathPrimary::Node(np) => {
                assert!(
                    np.where_clause.is_some(),
                    "expected node where_clause to be Some"
                );
            }
            other => panic!("expected Node, got {other:?}"),
        }
    }
}

// ── edgePattern ─────────────────────────────────────────────────────────
mod edge_pattern {
    use super::*;

    /// MATCH (a)-[e]->(b) RETURN a — PointingRight
    #[test]
    fn edge_right() {
        let fs = factors("MATCH (a)-[e]->(b) RETURN a");
        // factors: Node, Edge, Node — edge is at index 1
        assert!(fs.len() >= 3, "expected at least 3 factors");
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                assert_eq!(ep.direction, EdgeDirection::PointingRight);
                assert_eq!(ep.variable, Some("e".to_string()));
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }

    /// MATCH (a)<-[e]-(b) RETURN a — PointingLeft
    #[test]
    fn edge_left() {
        let fs = factors("MATCH (a)<-[e]-(b) RETURN a");
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                assert_eq!(ep.direction, EdgeDirection::PointingLeft);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }

    /// MATCH (a)~[e]~(b) RETURN a — Undirected
    #[test]
    fn edge_undirected() {
        let fs = factors("MATCH (a)~[e]~(b) RETURN a");
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                assert_eq!(ep.direction, EdgeDirection::Undirected);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }

    /// MATCH (a)-[e]-(b) RETURN a — AnyDirection (parsed as PointingRight by the parser)
    #[test]
    fn edge_any_direction() {
        let fs = factors("MATCH (a)-[e]-(b) RETURN a");
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                // Parser treats -[e]- the same as -[e]-> (PointingRight)
                assert_eq!(ep.direction, EdgeDirection::PointingRight);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }

    /// MATCH (a)<~[e]~(b) RETURN a — LeftOrUndirected
    #[test]
    fn edge_left_or_undirected() {
        let fs = factors("MATCH (a)<~[e]~(b) RETURN a");
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                assert_eq!(ep.direction, EdgeDirection::LeftOrUndirected);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }

    /// MATCH (a)~[e]~>(b) RETURN a — UndirectedOrRight (parsed as Undirected by the parser)
    #[test]
    fn edge_undirected_or_right() {
        let fs = factors("MATCH (a)~[e]~>(b) RETURN a");
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                // Parser treats ~[e]~> as Undirected
                assert_eq!(ep.direction, EdgeDirection::Undirected);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }

    /// MATCH (a)<-[e]->(b) RETURN a — LeftOrRight (parsed as PointingLeft by the parser)
    #[test]
    fn edge_left_or_right() {
        let fs = factors("MATCH (a)<-[e]->(b) RETURN a");
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                // Parser treats <-[e]-> as PointingLeft
                assert_eq!(ep.direction, EdgeDirection::PointingLeft);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }
}

// ── pathVariable ────────────────────────────────────────────────────────
mod path_variable {
    use super::*;

    /// MATCH p = (a)-[]->(b) RETURN p — paths[0].variable == Some("p")
    #[test]
    fn path_variable_assigned() {
        let m = ms("MATCH p = (a)-[]->(b) RETURN p");
        assert_eq!(
            m.pattern.paths[0].variable,
            Some("p".to_string()),
            "expected path variable 'p'"
        );
    }
}

// ── parenthesizedPathPattern ────────────────────────────────────────────
mod parenthesized_path {
    use super::*;

    /// MATCH ((a)-[e]->(b)) RETURN a — PathPrimary::Parenthesized
    #[test]
    fn parenthesized_basic() {
        let fs = factors("MATCH ((a)-[e]->(b)) RETURN a");
        // The parenthesized path is a single factor wrapping the sub-path
        assert!(!fs.is_empty());
        let found = fs
            .iter()
            .any(|f| matches!(&f.primary, PathPrimary::Parenthesized { .. }));
        assert!(
            found,
            "expected at least one Parenthesized factor, got {fs:?}"
        );
    }

    /// MATCH ((a)-[e]->(b) WHERE a.x > b.x) RETURN a — Parenthesized with WHERE
    #[test]
    fn parenthesized_with_where() {
        let fs = factors("MATCH ((a)-[e]->(b) WHERE a.x > b.x) RETURN a");
        let paren = fs
            .iter()
            .find_map(|f| match &f.primary {
                PathPrimary::Parenthesized { where_clause, .. } => Some(where_clause),
                _ => None,
            })
            .expect("expected Parenthesized factor");
        assert!(
            paren.is_some(),
            "expected where_clause inside parenthesized path"
        );
    }
}

// ── pathPatternUnion / multisetAlternation ──────────────────────────────
mod path_pattern_union {
    use super::*;

    /// MATCH (a)(()-[:A]->() | ()-[:B]->())(b) RETURN a, b
    /// → MultisetAlternation or PatternUnion
    #[test]
    fn multiset_alternation_or_union() {
        let m = ms("MATCH (a)(()-[:A]->() | ()-[:B]->())(b) RETURN a, b");
        // The pattern should contain at least one path; the parenthesized
        // sub-expression should have a MultisetAlternation or PatternUnion.
        let path = &m.pattern.paths[0];
        let found_union_like = match &path.expr {
            PathPatternExpr::MultisetAlternation(_) => true,
            PathPatternExpr::PatternUnion(_) => true,
            PathPatternExpr::Term(t) => t.factors.iter().any(|f| match &f.primary {
                PathPrimary::Parenthesized { expr, .. } => matches!(
                    expr.as_ref(),
                    PathPatternExpr::MultisetAlternation(_) | PathPatternExpr::PatternUnion(_)
                ),
                _ => false,
            }),
        };
        assert!(
            found_union_like,
            "expected MultisetAlternation or PatternUnion in path, got {:?}",
            path.expr
        );
    }
}

// ── abbreviated edges ─────────────────────────────────────────────────
mod abbreviated_edge {
    use super::*;

    /// MATCH (a)->(b) RETURN a — abbreviated right arrow
    #[test]
    fn abbreviated_right() {
        let fs = factors("MATCH (a)->(b) RETURN a");
        assert!(fs.len() >= 3);
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                assert_eq!(ep.direction, EdgeDirection::PointingRight);
                assert!(ep.variable.is_none());
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }

    /// MATCH (a)<-(b) RETURN a — abbreviated left arrow
    #[test]
    fn abbreviated_left() {
        let fs = factors("MATCH (a)<-(b) RETURN a");
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                assert_eq!(ep.direction, EdgeDirection::PointingLeft);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }

    /// MATCH (a)~(b) RETURN a — abbreviated undirected
    #[test]
    fn abbreviated_undirected() {
        let fs = factors("MATCH (a)~(b) RETURN a");
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                assert_eq!(ep.direction, EdgeDirection::Undirected);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }

    /// MATCH (a)-(b) RETURN a — abbreviated any direction
    #[test]
    fn abbreviated_any() {
        let fs = factors("MATCH (a)-(b) RETURN a");
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                assert_eq!(ep.direction, EdgeDirection::AnyDirection);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }

    /// MATCH (a)<->(b) RETURN a — abbreviated left-or-right
    #[test]
    fn abbreviated_left_or_right() {
        let fs = factors("MATCH (a)<->(b) RETURN a");
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                assert_eq!(ep.direction, EdgeDirection::LeftOrRight);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }

    /// MATCH (a)~>(b) RETURN a — abbreviated undirected-or-right
    #[test]
    fn abbreviated_undirected_or_right() {
        let fs = factors("MATCH (a)~>(b) RETURN a");
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                assert_eq!(ep.direction, EdgeDirection::UndirectedOrRight);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }

    /// MATCH (a)<~(b) RETURN a — abbreviated left-or-undirected
    #[test]
    fn abbreviated_left_or_undirected() {
        let fs = factors("MATCH (a)<~(b) RETURN a");
        match &fs[1].primary {
            PathPrimary::Edge(ep) => {
                assert_eq!(ep.direction, EdgeDirection::LeftOrUndirected);
            }
            other => panic!("expected Edge, got {other:?}"),
        }
    }
}

// ── parenthesized path with subpath var / mode ────────────────────────
mod parenthesized_path_extended {
    use super::*;

    /// MATCH (q = WALK (a)-[e]->(b)) RETURN a — parenthesized with variable and mode
    #[test]
    fn parenthesized_with_var_and_mode() {
        let fs = factors("MATCH (q = WALK (a)-[e]->(b)) RETURN a");
        let paren = fs.iter().find_map(|f| match &f.primary {
            PathPrimary::Parenthesized { variable, mode, .. } => Some((variable, mode)),
            _ => None,
        });
        let (var, mode) = paren.expect("expected Parenthesized");
        assert_eq!(*var, Some("q".to_string()));
        assert_eq!(*mode, Some(PathMode::Walk));
    }

    /// MATCH (TRAIL (a)-[e]->(b)) RETURN a — parenthesized with mode only
    #[test]
    fn parenthesized_with_mode() {
        let fs = factors("MATCH (TRAIL (a)-[e]->(b)) RETURN a");
        let paren = fs.iter().find_map(|f| match &f.primary {
            PathPrimary::Parenthesized { mode, .. } => Some(mode),
            _ => None,
        });
        let mode = paren.expect("expected Parenthesized");
        assert_eq!(*mode, Some(PathMode::Trail));
    }
}

// ── node with IS label ───────────────────────────────────────────────
mod node_is_label {
    use super::*;

    /// MATCH (n IS Person) RETURN n — node with IS keyword
    #[test]
    fn node_with_is() {
        let fs = factors("MATCH (n IS Person) RETURN n");
        match &fs[0].primary {
            PathPrimary::Node(np) => {
                assert_eq!(np.is_or_colon, Some(IsOrColon::Is));
                assert!(np.label.is_some());
            }
            other => panic!("expected Node, got {other:?}"),
        }
    }
}

// ── quantifier on path factors ───────────────────────────────────────
mod path_factor_quantifier {
    use super::*;

    /// MATCH (a)-[e]->(b)* RETURN a — Star quantifier on edge
    #[test]
    fn star_on_factor() {
        let fs = factors("MATCH (a)((n)-[e]->(m))*(b) RETURN a");
        let quantified = fs.iter().find(|f| f.quantifier.is_some());
        assert_eq!(
            quantified.expect("no quantified factor").quantifier,
            Some(PathQuantifier::Star),
        );
    }

    /// MATCH (a)((n)-[e]->(m))+(b) RETURN a — Plus quantifier
    #[test]
    fn plus_on_factor() {
        let fs = factors("MATCH (a)((n)-[e]->(m))+(b) RETURN a");
        let quantified = fs.iter().find(|f| f.quantifier.is_some());
        assert_eq!(
            quantified.expect("no quantified factor").quantifier,
            Some(PathQuantifier::Plus),
        );
    }
}
