//! §16.8 — Label expressions.

use super::helpers::graph_pat;
use crate::section_tests::p;

// ── Parenthesized label expression ──────────────────────────────────────

#[test]
fn parenthesized_label() {
    let prog = p("MATCH (n :(Person|Employee)) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}

/// ISO 16.8: `labelExpressionNegation : EXCLAMATION_MARK labelExpression`, so negation nests.
/// `s16.8-label-expression-nested-negation` in the conformance ledger.
#[test]
fn nested_label_negation() {
    use gleaph_gql::ast::{PathPatternExpr, PathPrimary};
    use gleaph_gql::types::LabelExpr;

    let prog = p("MATCH (n:!!A) RETURN n");
    let gp = graph_pat(&prog);
    let PathPatternExpr::Term(term) = &gp.paths[0].expr else {
        panic!("expected a single path term");
    };
    let label = term
        .factors
        .iter()
        .find_map(|factor| match &factor.primary {
            PathPrimary::Node(node) => node.label.as_ref(),
            _ => None,
        })
        .expect("expected a labeled node pattern");
    let LabelExpr::Not(inner) = label else {
        panic!("expected outer negation, got {label:?}");
    };
    let LabelExpr::Not(innermost) = inner.as_ref() else {
        panic!("expected nested negation, got {inner:?}");
    };
    assert_eq!(innermost.as_ref(), &LabelExpr::Name("A".into()));
}
