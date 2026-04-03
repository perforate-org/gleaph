//! §16.8 — Label expression.
//!
//! GQL rules: labelExpression, labelTerm, labelFactor, labelPrimary.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::LabelExpr;

/// Helper to extract the label from the first node pattern.
fn node_label(input: &str) -> LabelExpr {
    let prog = p(input);
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            for part in &cq.left.parts {
                if let SimpleQueryStatement::Match(m) = part
                    && let PathPatternExpr::Term(t) = &m.pattern.paths[0].expr
                {
                    for factor in &t.factors {
                        if let PathPrimary::Node(np) = &factor.primary {
                            return np.label.clone().expect("expected label on node pattern");
                        }
                    }
                }
            }
            panic!("no node pattern found");
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

// ── labelExpression ─────────────────────────────────────────────────────
mod label_expression {
    use super::*;

    /// MATCH (n:Person) RETURN n → LabelExpr::Name("Person")
    #[test]
    fn single_label() {
        let label = node_label("MATCH (n:Person) RETURN n");
        assert_eq!(label, LabelExpr::Name("Person".to_string()));
    }

    /// MATCH (n:Person&Employee) RETURN n → LabelExpr::And
    #[test]
    fn conjunction() {
        let label = node_label("MATCH (n:Person&Employee) RETURN n");
        match label {
            LabelExpr::And(left, right) => {
                assert_eq!(*left, LabelExpr::Name("Person".to_string()));
                assert_eq!(*right, LabelExpr::Name("Employee".to_string()));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    /// MATCH (n:Person|Company) RETURN n → LabelExpr::Or
    #[test]
    fn disjunction() {
        let label = node_label("MATCH (n:Person|Company) RETURN n");
        match label {
            LabelExpr::Or(left, right) => {
                assert_eq!(*left, LabelExpr::Name("Person".to_string()));
                assert_eq!(*right, LabelExpr::Name("Company".to_string()));
            }
            other => panic!("expected Or, got {other:?}"),
        }
    }

    /// MATCH (n:!Person) RETURN n → LabelExpr::Not
    #[test]
    fn negation() {
        let label = node_label("MATCH (n:!Person) RETURN n");
        match label {
            LabelExpr::Not(inner) => {
                assert_eq!(*inner, LabelExpr::Name("Person".to_string()));
            }
            other => panic!("expected Not, got {other:?}"),
        }
    }

    /// MATCH (n:%) RETURN n → LabelExpr::Wildcard
    #[test]
    fn wildcard() {
        let label = node_label("MATCH (n:%) RETURN n");
        assert_eq!(label, LabelExpr::Wildcard);
    }
}
