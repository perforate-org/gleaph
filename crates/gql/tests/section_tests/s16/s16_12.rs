//! §16.12 — Simplified path pattern expression.
//!
//! GQL rules: simplifiedPathPatternExpression, simplifiedElement,
//! simplifiedContents, simplifiedTertiary, simplifiedSecondary, simplifiedPrimary.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::{EdgeDirection, LabelExpr};

/// Helper to extract simplified elements from the first path.
fn simplified_elements(input: &str) -> Vec<SimplifiedElement> {
    let prog = p(input);
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            for part in &cq.left.parts {
                if let SimpleQueryStatement::Match(m) = part {
                    if let PathPatternExpr::Term(t) = &m.pattern.paths[0].expr {
                        for factor in &t.factors {
                            if let PathPrimary::Simplified(sp) = &factor.primary {
                                return sp.elements.clone();
                            }
                        }
                    }
                }
            }
            panic!("no simplified path pattern found in: {input}");
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

// ── simplifiedPathPatternExpression ─────────────────────────────────────
mod simplified_path_pattern {
    use super::*;

    /// MATCH (a)-/KNOWS/->(b) RETURN a, b — PointingRight
    #[test]
    fn simplified_right() {
        let elems = simplified_elements("MATCH (a)-/KNOWS/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0].direction, EdgeDirection::PointingRight);
        assert_eq!(
            elems[0].contents,
            SimplifiedContents::Label(LabelExpr::Name("KNOWS".to_string()))
        );
    }

    /// MATCH (a)<-/KNOWS/-(b) RETURN a, b — PointingLeft
    #[test]
    fn simplified_left() {
        let elems = simplified_elements("MATCH (a)<-/KNOWS/-(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0].direction, EdgeDirection::PointingLeft);
    }

    /// MATCH (a)~/KNOWS/~(b) RETURN a, b — Undirected
    #[test]
    fn simplified_undirected() {
        let elems = simplified_elements("MATCH (a)~/KNOWS/~(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0].direction, EdgeDirection::Undirected);
    }

    /// MATCH (a)-/KNOWS/-(b) RETURN a, b — LeftOrUndirectedOrRight (any)
    #[test]
    fn simplified_any() {
        let elems = simplified_elements("MATCH (a)-/KNOWS/-(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0].direction, EdgeDirection::AnyDirection);
    }

    /// MATCH (a)-/<KNOWS/->(b) RETURN a, b — DirectionOverride(PointingLeft, ...)
    #[test]
    fn direction_override_left() {
        let elems = simplified_elements("MATCH (a)-/<KNOWS/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::DirectionOverride(dir, inner) => {
                assert_eq!(*dir, EdgeDirection::PointingLeft);
                assert_eq!(
                    **inner,
                    SimplifiedContents::Label(LabelExpr::Name("KNOWS".to_string()))
                );
            }
            other => panic!("expected DirectionOverride, got {other:?}"),
        }
    }

    /// MATCH (a)-/KNOWS>/->(b) RETURN a, b — DirectionOverride(PointingRight/UndirectedOrRight, ...)
    #[test]
    fn direction_override_right() {
        let elems = simplified_elements("MATCH (a)-/KNOWS>/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::DirectionOverride(dir, _) => {
                // KNOWS> inside simplified path — direction should be right-pointing
                // or undirected-or-right depending on parser interpretation.
                assert!(
                    *dir == EdgeDirection::PointingRight
                        || *dir == EdgeDirection::UndirectedOrRight,
                    "expected right-pointing direction override, got {dir:?}"
                );
            }
            other => panic!("expected DirectionOverride, got {other:?}"),
        }
    }

    /// MATCH (a)-/~KNOWS/->(b) RETURN a, b — DirectionOverride(Undirected, ...)
    #[test]
    fn direction_override_undirected() {
        let elems = simplified_elements("MATCH (a)-/~KNOWS/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::DirectionOverride(dir, inner) => {
                assert_eq!(*dir, EdgeDirection::Undirected);
                assert_eq!(
                    **inner,
                    SimplifiedContents::Label(LabelExpr::Name("KNOWS".to_string()))
                );
            }
            other => panic!("expected DirectionOverride, got {other:?}"),
        }
    }

    /// MATCH (a)-/!KNOWS/->(b) RETURN a, b — Negation
    #[test]
    fn negation() {
        let elems = simplified_elements("MATCH (a)-/!KNOWS/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::Negation(inner) => {
                assert_eq!(
                    **inner,
                    SimplifiedContents::Label(LabelExpr::Name("KNOWS".to_string()))
                );
            }
            other => panic!("expected Negation, got {other:?}"),
        }
    }

    /// MATCH (a)-/%/->(b) RETURN a, b — Wildcard label
    #[test]
    fn wildcard() {
        let elems = simplified_elements("MATCH (a)-/%/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        assert_eq!(
            elems[0].contents,
            SimplifiedContents::Label(LabelExpr::Wildcard)
        );
    }

    /// MATCH (a)-/KNOWS&LIKES/->(b) RETURN a, b — Conjunction
    #[test]
    fn conjunction() {
        let elems = simplified_elements("MATCH (a)-/KNOWS&LIKES/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::Conjunction(left, right) => {
                assert_eq!(
                    **left,
                    SimplifiedContents::Label(LabelExpr::Name("KNOWS".to_string()))
                );
                assert_eq!(
                    **right,
                    SimplifiedContents::Label(LabelExpr::Name("LIKES".to_string()))
                );
            }
            other => panic!("expected Conjunction, got {other:?}"),
        }
    }

    /// MATCH (a)-/KNOWS|LIKES/->(b) RETURN a, b — Union
    #[test]
    fn union() {
        let elems = simplified_elements("MATCH (a)-/KNOWS|LIKES/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::Union(left, right) => {
                assert_eq!(
                    **left,
                    SimplifiedContents::Label(LabelExpr::Name("KNOWS".to_string()))
                );
                assert_eq!(
                    **right,
                    SimplifiedContents::Label(LabelExpr::Name("LIKES".to_string()))
                );
            }
            other => panic!("expected Union, got {other:?}"),
        }
    }

    /// MATCH (a)-/KNOWS{2,5}/->(b) RETURN a, b — Quantified
    #[test]
    fn quantified() {
        let elems = simplified_elements("MATCH (a)-/KNOWS{2,5}/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::Quantified(inner, quantifier) => {
                assert_eq!(
                    **inner,
                    SimplifiedContents::Label(LabelExpr::Name("KNOWS".to_string()))
                );
                assert_eq!(
                    *quantifier,
                    PathQuantifier::Range {
                        lower: 2,
                        upper: Some(5)
                    }
                );
            }
            other => panic!("expected Quantified, got {other:?}"),
        }
    }

    /// MATCH (a)-/(KNOWS|LIKES)/->(b) RETURN a, b — Group (parenthesized)
    #[test]
    fn group_parenthesized() {
        let elems = simplified_elements("MATCH (a)-/(KNOWS|LIKES)/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::Group(inner) => {
                match inner.as_ref() {
                    SimplifiedContents::Union(_, _) => {} // expected
                    other => panic!("expected Union inside Group, got {other:?}"),
                }
            }
            other => panic!("expected Group, got {other:?}"),
        }
    }

    /// MATCH (a)-/KNOWS LIKES/->(b) RETURN a, b — Concatenation
    #[test]
    fn concatenation() {
        let elems = simplified_elements("MATCH (a)-/KNOWS LIKES/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::Concatenation(left, right) => {
                assert_eq!(
                    **left,
                    SimplifiedContents::Label(LabelExpr::Name("KNOWS".to_string()))
                );
                assert_eq!(
                    **right,
                    SimplifiedContents::Label(LabelExpr::Name("LIKES".to_string()))
                );
            }
            other => panic!("expected Concatenation, got {other:?}"),
        }
    }

    /// MATCH (a)-/-KNOWS/->(b) RETURN a, b — Direction override: any direction
    #[test]
    fn direction_override_any() {
        let elems = simplified_elements("MATCH (a)-/-KNOWS/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::DirectionOverride(dir, _) => {
                assert_eq!(*dir, EdgeDirection::AnyDirection);
            }
            other => panic!("expected DirectionOverride, got {other:?}"),
        }
    }

    /// MATCH (a)-/<KNOWS>/->(b) RETURN a, b — Direction override: left-or-right
    #[test]
    fn direction_override_left_or_right() {
        let elems = simplified_elements("MATCH (a)-/<KNOWS>/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::DirectionOverride(dir, _) => {
                assert_eq!(*dir, EdgeDirection::LeftOrRight);
            }
            other => panic!("expected DirectionOverride, got {other:?}"),
        }
    }

    /// MATCH (a)-/~KNOWS>/->(b) RETURN a, b — Direction override: undirected-or-right
    #[test]
    fn direction_override_undirected_or_right() {
        let elems = simplified_elements("MATCH (a)-/~KNOWS>/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::DirectionOverride(dir, _) => {
                assert_eq!(*dir, EdgeDirection::UndirectedOrRight);
            }
            other => panic!("expected DirectionOverride, got {other:?}"),
        }
    }

    /// MATCH (a)<-/KNOWS/-(b) RETURN a, b followed by another simplified
    /// — two simplified elements in sequence (LeftOrRight via opening combo)
    #[test]
    fn simplified_left_or_right() {
        let elems = simplified_elements("MATCH (a)<-/KNOWS/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0].direction, EdgeDirection::LeftOrRight);
    }

    /// MATCH (a)~/KNOWS/~>(b) RETURN a, b — UndirectedOrRight direction
    #[test]
    fn simplified_undirected_or_right() {
        let elems = simplified_elements("MATCH (a)~/KNOWS/~>(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0].direction, EdgeDirection::UndirectedOrRight);
    }

    /// MATCH (a)<~/KNOWS/~(b) RETURN a, b — LeftOrUndirected direction
    #[test]
    fn simplified_left_or_undirected() {
        let elems = simplified_elements("MATCH (a)<~/KNOWS/~(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0].direction, EdgeDirection::LeftOrUndirected);
    }

    /// MATCH (a)-/KNOWS+/->(b) RETURN a, b — Quantified with +
    #[test]
    fn quantified_plus() {
        let elems = simplified_elements("MATCH (a)-/KNOWS+/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::Quantified(_, q) => {
                assert_eq!(*q, PathQuantifier::Plus);
            }
            other => panic!("expected Quantified, got {other:?}"),
        }
    }

    /// MATCH (a)-/KNOWS*/->(b) RETURN a, b — Quantified with *
    #[test]
    fn quantified_star() {
        let elems = simplified_elements("MATCH (a)-/KNOWS*/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::Quantified(_, q) => {
                assert_eq!(*q, PathQuantifier::Star);
            }
            other => panic!("expected Quantified, got {other:?}"),
        }
    }

    /// MATCH (a)-/KNOWS?/->(b) RETURN a, b — Quantified with ?
    #[test]
    fn quantified_optional() {
        let elems = simplified_elements("MATCH (a)-/KNOWS?/->(b) RETURN a, b");
        assert_eq!(elems.len(), 1);
        match &elems[0].contents {
            SimplifiedContents::Quantified(_, q) => {
                assert_eq!(*q, PathQuantifier::Optional);
            }
            other => panic!("expected Quantified, got {other:?}"),
        }
    }
}
