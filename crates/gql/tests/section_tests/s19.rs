//! §19 — Predicates.
//!
//! GQL rules: comparisonPredicate, existsPredicate, nullPredicate,
//! valueTypePredicate, normalizedPredicate, directedPredicate, labeledPredicate,
//! sourceDestinationPredicate, allDifferentPredicate, samePredicate,
//! propertyExistsPredicate.

use crate::section_tests::p;
use gleaph_gql::ast::*;

/// Extract the WHERE condition from `MATCH (n) WHERE <cond> RETURN n`.
fn where_expr(prog: &GqlProgram) -> &Expr {
    let b = crate::section_tests::body(prog);
    match &b.first {
        Statement::Query(cq) => match &cq.left.parts[0] {
            SimpleQueryStatement::Match(m) => m.pattern.where_clause.as_ref().unwrap(),
            other => panic!("expected Match, got {other:?}"),
        },
        other => panic!("expected Query, got {other:?}"),
    }
}

// ── comparisonPredicate ─────────────────────────────────────────────────
//   §19.3 — Comparison predicates: =, <>, <, >, <=, >=
mod comparison_predicate {
    use super::*;

    #[test]
    fn eq() {
        let prog = p("MATCH (n) WHERE n.x = 1 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Compare { op, left, right } => {
                assert_eq!(*op, CmpOp::Eq);
                assert!(
                    matches!(left.as_ref().kind, ExprKind::PropertyAccess { ref property, .. } if property == "x")
                );
                assert_eq!(*right.as_ref(), Expr::int(1));
            }
            other => panic!("expected Compare, got {other:?}"),
        }
    }

    #[test]
    fn ne() {
        let prog = p("MATCH (n) WHERE n.x <> 1 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Compare { op, .. } => assert_eq!(*op, CmpOp::Ne),
            other => panic!("expected Compare, got {other:?}"),
        }
    }

    #[test]
    fn lt() {
        let prog = p("MATCH (n) WHERE n.x < 1 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Compare { op, .. } => assert_eq!(*op, CmpOp::Lt),
            other => panic!("expected Compare, got {other:?}"),
        }
    }

    #[test]
    fn gt() {
        let prog = p("MATCH (n) WHERE n.x > 1 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Compare { op, .. } => assert_eq!(*op, CmpOp::Gt),
            other => panic!("expected Compare, got {other:?}"),
        }
    }

    #[test]
    fn le() {
        let prog = p("MATCH (n) WHERE n.x <= 1 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Compare { op, .. } => assert_eq!(*op, CmpOp::Le),
            other => panic!("expected Compare, got {other:?}"),
        }
    }

    #[test]
    fn ge() {
        let prog = p("MATCH (n) WHERE n.x >= 1 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Compare { op, .. } => assert_eq!(*op, CmpOp::Ge),
            other => panic!("expected Compare, got {other:?}"),
        }
    }
}

// ── existsPredicate ─────────────────────────────────────────────────────
//   §19.4 — EXISTS { pattern } and EXISTS { subquery }
mod exists_predicate {
    use super::*;

    /// EXISTS { (n)-[:KNOWS]->() } — pattern form
    #[test]
    fn exists_pattern() {
        let prog = p("MATCH (n) WHERE EXISTS { (n)-[:KNOWS]->() } RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::ExistsPattern(_) => {}
            other => panic!("expected ExistsPattern, got {other:?}"),
        }
    }

    /// EXISTS { MATCH (n)-[:KNOWS]->(m) RETURN m } — subquery form
    #[test]
    fn exists_subquery() {
        let prog = p("MATCH (n) WHERE EXISTS { MATCH (n)-[:KNOWS]->(m) RETURN m } RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::ExistsSubquery(_) => {}
            other => panic!("expected ExistsSubquery, got {other:?}"),
        }
    }
}

// ── nullPredicate ───────────────────────────────────────────────────────
//   §19.5 — IS NULL / IS NOT NULL
mod null_predicate {
    use super::*;

    #[test]
    fn is_null() {
        let prog = p("MATCH (n) WHERE n.x IS NULL RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsNull(inner) => {
                assert!(
                    matches!(inner.as_ref().kind, ExprKind::PropertyAccess { ref property, .. } if property == "x")
                );
            }
            other => panic!("expected IsNull, got {other:?}"),
        }
    }

    #[test]
    fn is_not_null() {
        let prog = p("MATCH (n) WHERE n.x IS NOT NULL RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsNotNull(inner) => {
                assert!(
                    matches!(inner.as_ref().kind, ExprKind::PropertyAccess { ref property, .. } if property == "x")
                );
            }
            other => panic!("expected IsNotNull, got {other:?}"),
        }
    }
}

// ── valueTypePredicate ──────────────────────────────────────────────────
//   §19.6 — IS TYPED / IS NOT TYPED
mod value_type_predicate {
    use super::*;

    #[test]
    fn is_typed_int32() {
        let prog = p("MATCH (n) WHERE n.x IS TYPED INT32 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsTyped {
                negated, target, ..
            } => {
                assert!(!negated);
                assert!(matches!(*target, ValueType::Int32 { .. }));
            }
            other => panic!("expected IsTyped, got {other:?}"),
        }
    }

    #[test]
    fn is_not_typed_string() {
        let prog = p("MATCH (n) WHERE n.x IS NOT TYPED STRING RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsTyped {
                negated, target, ..
            } => {
                assert!(negated);
                assert_eq!(
                    *target,
                    ValueType::String {
                        min_length: None,
                        max_length: None
                    }
                );
            }
            other => panic!("expected IsTyped, got {other:?}"),
        }
    }
}

// ── normalizedPredicate ─────────────────────────────────────────────────
//   §19.7 — IS [NOT] [form] NORMALIZED
mod normalized_predicate {
    use super::*;

    #[test]
    fn is_normalized_default_nfc() {
        let prog = p("MATCH (n) WHERE n.x IS NORMALIZED RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsNormalized { form, negated, .. } => {
                assert!(!negated);
                assert_eq!(*form, NormalForm::NFC);
            }
            other => panic!("expected IsNormalized, got {other:?}"),
        }
    }

    #[test]
    fn is_nfc_normalized() {
        let prog = p("MATCH (n) WHERE n.x IS NFC NORMALIZED RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsNormalized { form, .. } => {
                assert_eq!(*form, NormalForm::NFC);
            }
            other => panic!("expected IsNormalized, got {other:?}"),
        }
    }

    #[test]
    fn is_nfkd_normalized() {
        let prog = p("MATCH (n) WHERE n.x IS NFKD NORMALIZED RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsNormalized { form, .. } => {
                assert_eq!(*form, NormalForm::NFKD);
            }
            other => panic!("expected IsNormalized, got {other:?}"),
        }
    }

    #[test]
    fn is_not_normalized() {
        let prog = p("MATCH (n) WHERE n.x IS NOT NORMALIZED RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsNormalized { negated, .. } => {
                assert!(negated);
            }
            other => panic!("expected IsNormalized, got {other:?}"),
        }
    }
}

// ── directedPredicate ───────────────────────────────────────────────────
//   §19.8 — IS DIRECTED / IS NOT DIRECTED
mod directed_predicate {
    use super::*;

    #[test]
    fn is_directed() {
        let prog = p("MATCH (n) WHERE n IS DIRECTED RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsDirected { negated, .. } => {
                assert!(!negated);
            }
            other => panic!("expected IsDirected, got {other:?}"),
        }
    }

    #[test]
    fn is_not_directed() {
        let prog = p("MATCH (n) WHERE n IS NOT DIRECTED RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsDirected { negated, .. } => {
                assert!(negated);
            }
            other => panic!("expected IsDirected, got {other:?}"),
        }
    }
}

// ── labeledPredicate ────────────────────────────────────────────────────
//   §19.9 — IS LABELED / IS NOT LABELED
mod labeled_predicate {
    use super::*;
    use gleaph_gql::types::LabelExpr;

    #[test]
    fn is_labeled() {
        let prog = p("MATCH (n) WHERE n IS LABELED Person RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsLabeled { negated, label, .. } => {
                assert!(!negated);
                assert_eq!(*label, LabelExpr::Name("Person".to_string()));
            }
            other => panic!("expected IsLabeled, got {other:?}"),
        }
    }

    #[test]
    fn is_not_labeled() {
        let prog = p("MATCH (n) WHERE n IS NOT LABELED Person RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsLabeled { negated, label, .. } => {
                assert!(negated);
                assert_eq!(*label, LabelExpr::Name("Person".to_string()));
            }
            other => panic!("expected IsLabeled, got {other:?}"),
        }
    }
}

// ── sourceDestinationPredicate ──────────────────────────────────────────
//   §19.10 — IS SOURCE OF / IS NOT SOURCE OF / IS DESTINATION OF
mod source_destination_predicate {
    use super::*;

    #[test]
    fn is_source_of() {
        let prog = p("MATCH (n)-[e]->(m) WHERE n IS SOURCE OF e RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsSourceOf { negated, .. } => {
                assert!(!negated);
            }
            other => panic!("expected IsSourceOf, got {other:?}"),
        }
    }

    #[test]
    fn is_not_source_of() {
        let prog = p("MATCH (n)-[e]->(m) WHERE n IS NOT SOURCE OF e RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsSourceOf { negated, .. } => {
                assert!(negated);
            }
            other => panic!("expected IsSourceOf, got {other:?}"),
        }
    }

    #[test]
    fn is_destination_of() {
        let prog = p("MATCH (n)-[e]->(m) WHERE m IS DESTINATION OF e RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::IsDestOf { negated, .. } => {
                assert!(!negated);
            }
            other => panic!("expected IsDestOf, got {other:?}"),
        }
    }
}

// ── allDifferentPredicate ───────────────────────────────────────────────
//   §19.11 — ALL_DIFFERENT(a, b, ...)
mod all_different_predicate {
    use super::*;

    #[test]
    fn two_elements() {
        let prog = p("MATCH (a), (b) WHERE ALL_DIFFERENT(a, b) RETURN a");
        match &where_expr(&prog).kind {
            ExprKind::AllDifferent(args) => {
                assert_eq!(args.len(), 2);
                assert_eq!(args[0], Expr::var("a"));
                assert_eq!(args[1], Expr::var("b"));
            }
            other => panic!("expected AllDifferent, got {other:?}"),
        }
    }
}

// ── samePredicate ───────────────────────────────────────────────────────
//   §19.12 — SAME(a, b, ...)
mod same_predicate {
    use super::*;

    #[test]
    fn two_elements() {
        let prog = p("MATCH (a), (b) WHERE SAME(a, b) RETURN a");
        match &where_expr(&prog).kind {
            ExprKind::Same(args) => {
                assert_eq!(args.len(), 2);
                assert_eq!(args[0], Expr::var("a"));
                assert_eq!(args[1], Expr::var("b"));
            }
            other => panic!("expected Same, got {other:?}"),
        }
    }
}

// ── propertyExistsPredicate ─────────────────────────────────────────────
//   §19.13 — PROPERTY_EXISTS(n, prop)
mod property_exists_predicate {
    use super::*;

    #[test]
    fn property_exists() {
        let prog = p("MATCH (n) WHERE PROPERTY_EXISTS(n, name) RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::PropertyExists { property, .. } => {
                assert_eq!(property, "name");
            }
            other => panic!("expected PropertyExists, got {other:?}"),
        }
    }
}
