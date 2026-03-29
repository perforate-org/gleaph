//! §20.20 — Boolean value expression.
//!
//! GQL rules: booleanValueExpression, booleanTerm, booleanFactor,
//! booleanTest, truthValue.

use crate::section_tests::p;
use gleaph_gql::ast::*;

/// Extract the WHERE condition.
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

/// Extract the first return item expression.
fn ret_expr(prog: &GqlProgram) -> &Expr {
    let b = crate::section_tests::body(prog);
    match &b.first {
        Statement::Query(cq) => match cq.left.result.as_ref().unwrap() {
            ResultStatement::Return(ret) => match &ret.body {
                ReturnBody::Items { items, .. } => &items[0].expr,
                other => panic!("expected Items, got {other:?}"),
            },
            other => panic!("expected Return, got {other:?}"),
        },
        other => panic!("expected Query, got {other:?}"),
    }
}

// ── booleanValueExpression ──────────────────────────────────────────────
mod boolean_value_expression {
    use super::*;

    /// a AND b
    #[test]
    fn and() {
        let prog = p("MATCH (n) WHERE n.x = 1 AND n.y = 2 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::And(left, right) => {
                assert!(matches!(
                    &left.as_ref().kind,
                    ExprKind::Compare { op: CmpOp::Eq, .. }
                ));
                assert!(matches!(
                    &right.as_ref().kind,
                    ExprKind::Compare { op: CmpOp::Eq, .. }
                ));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    /// a OR b
    #[test]
    fn or() {
        let prog = p("MATCH (n) WHERE n.x = 1 OR n.y = 2 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Or(left, right) => {
                assert!(matches!(
                    &left.as_ref().kind,
                    ExprKind::Compare { op: CmpOp::Eq, .. }
                ));
                assert!(matches!(
                    &right.as_ref().kind,
                    ExprKind::Compare { op: CmpOp::Eq, .. }
                ));
            }
            other => panic!("expected Or, got {other:?}"),
        }
    }

    /// NOT a
    #[test]
    fn not() {
        let prog = p("MATCH (n) WHERE NOT n.x = 1 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Not(inner) => {
                assert!(matches!(
                    &inner.as_ref().kind,
                    ExprKind::Compare { op: CmpOp::Eq, .. }
                ));
            }
            other => panic!("expected Not, got {other:?}"),
        }
    }

    /// a XOR b
    #[test]
    fn xor() {
        let prog = p("MATCH (n) WHERE n.x = 1 XOR n.y = 2 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Xor(_, _) => {}
            other => panic!("expected Xor, got {other:?}"),
        }
    }
}

// ── booleanTest / truthValue ────────────────────────────────────────────
mod boolean_test {
    use super::*;

    /// expr IS TRUE
    #[test]
    fn is_true() {
        let prog = p("MATCH (n) RETURN n.active IS TRUE");
        match &ret_expr(&prog).kind {
            ExprKind::IsTruth { value, negated, .. } => {
                assert_eq!(*value, TruthValue::True);
                assert!(!negated);
            }
            other => panic!("expected IsTruth, got {other:?}"),
        }
    }

    /// expr IS FALSE
    #[test]
    fn is_false() {
        let prog = p("MATCH (n) RETURN n.active IS FALSE");
        match &ret_expr(&prog).kind {
            ExprKind::IsTruth { value, .. } => {
                assert_eq!(*value, TruthValue::False);
            }
            other => panic!("expected IsTruth, got {other:?}"),
        }
    }

    /// expr IS NOT TRUE
    #[test]
    fn is_not_true() {
        let prog = p("MATCH (n) RETURN n.active IS NOT TRUE");
        match &ret_expr(&prog).kind {
            ExprKind::IsTruth { value, negated, .. } => {
                assert_eq!(*value, TruthValue::True);
                assert!(negated);
            }
            other => panic!("expected IsTruth, got {other:?}"),
        }
    }
}
