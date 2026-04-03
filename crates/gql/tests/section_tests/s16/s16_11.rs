//! §16.11 — Graph pattern quantifier.
//!
//! GQL rule: `graphPatternQuantifier : ASTERISK | PLUS_SIGN | QUESTION_MARK
//!              | LEFT_BRACE fixedQuantifier RIGHT_BRACE
//!              | LEFT_BRACE generalQuantifier RIGHT_BRACE`

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

/// Helper to extract the quantifier from a parenthesized sub-path.
/// We use patterns like: MATCH ((n)-[e]->(m)){quantifier}(x) RETURN n
/// to produce a Parenthesized factor with a quantifier.
fn quantifier_from(input: &str) -> PathQuantifier {
    let prog = p(input);
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            for part in &cq.left.parts {
                if let SimpleQueryStatement::Match(m) = part
                    && let PathPatternExpr::Term(t) = &m.pattern.paths[0].expr
                {
                    for factor in &t.factors {
                        if factor.quantifier.is_some() {
                            return factor.quantifier.clone().unwrap();
                        }
                    }
                }
            }
            panic!("no quantifier found in query: {input}");
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

// ── graphPatternQuantifier ──────────────────────────────────────────────
mod graph_pattern_quantifier {
    use super::*;

    /// ((n)-[e]->(m)){0,} — Star (zero or more)
    #[test]
    fn star() {
        let q = quantifier_from("MATCH ((n)-[e]->(m)){0,} RETURN n");
        assert_eq!(
            q,
            PathQuantifier::Range {
                lower: 0,
                upper: None
            },
            "expected Range {{ lower: 0, upper: None }} for star"
        );
    }

    /// ((n)-[e]->(m)){1,} — Plus (one or more)
    #[test]
    fn plus() {
        let q = quantifier_from("MATCH ((n)-[e]->(m)){1,} RETURN n");
        assert_eq!(
            q,
            PathQuantifier::Range {
                lower: 1,
                upper: None
            },
            "expected Range {{ lower: 1, upper: None }} for plus"
        );
    }

    /// ((n)-[e]->(m))? — Optional (zero or one)
    #[test]
    fn optional() {
        let q = quantifier_from("MATCH (a)((n)-[e]->(m))?(b) RETURN a");
        assert_eq!(q, PathQuantifier::Optional, "expected Optional");
    }

    /// ((n)-[e]->(m)){3} — Fixed(3)
    #[test]
    fn fixed() {
        let q = quantifier_from("MATCH ((n)-[e]->(m)){3} RETURN n");
        assert_eq!(q, PathQuantifier::Fixed(3), "expected Fixed(3)");
    }

    /// ((n)-[e]->(m)){2,5} — Range { lower: 2, upper: Some(5) }
    #[test]
    fn range() {
        let q = quantifier_from("MATCH ((n)-[e]->(m)){2,5} RETURN n");
        assert_eq!(
            q,
            PathQuantifier::Range {
                lower: 2,
                upper: Some(5)
            },
            "expected Range {{ lower: 2, upper: Some(5) }}"
        );
    }

    /// ((n)-[e]->(m)){,3} — Range with no lower bound
    #[test]
    fn range_no_lower() {
        let q = quantifier_from("MATCH ((n)-[e]->(m)){,3} RETURN n");
        assert_eq!(
            q,
            PathQuantifier::Range {
                lower: 0,
                upper: Some(3)
            },
        );
    }

    /// ((n)-[e]->(m))* — Star quantifier
    #[test]
    fn star_symbol() {
        let q = quantifier_from("MATCH (a)((n)-[e]->(m))*(b) RETURN a");
        assert_eq!(q, PathQuantifier::Star);
    }

    /// ((n)-[e]->(m))+ — Plus quantifier
    #[test]
    fn plus_symbol() {
        let q = quantifier_from("MATCH (a)((n)-[e]->(m))+(b) RETURN a");
        assert_eq!(q, PathQuantifier::Plus);
    }
}
