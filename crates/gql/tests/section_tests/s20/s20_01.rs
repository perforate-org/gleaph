//! §20.1 — Arithmetic expressions.
//!
//! GQL rules: additiveExpression, multiplicativeExpression, unaryExpression.

use crate::section_tests::p;
use gleaph_gql::ast::*;

/// Extract the first return item expression from `MATCH (n) RETURN <expr>`.
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

// ── additiveExpression / multiplicativeExpression ────────────────────────
mod arithmetic {
    use super::*;

    #[test]
    fn add() {
        let prog = p("MATCH (n) RETURN n.x + n.y");
        match &ret_expr(&prog).kind {
            ExprKind::BinaryOp { op, left, right } => {
                assert_eq!(*op, BinaryOp::Add);
                assert!(
                    matches!(&left.as_ref().kind, ExprKind::PropertyAccess { property, .. } if property == "x")
                );
                assert!(
                    matches!(&right.as_ref().kind, ExprKind::PropertyAccess { property, .. } if property == "y")
                );
            }
            other => panic!("expected BinaryOp, got {other:?}"),
        }
    }

    #[test]
    fn sub() {
        let prog = p("MATCH (n) RETURN n.x - n.y");
        match &ret_expr(&prog).kind {
            ExprKind::BinaryOp { op, .. } => assert_eq!(*op, BinaryOp::Sub),
            other => panic!("expected BinaryOp, got {other:?}"),
        }
    }

    #[test]
    fn mul() {
        let prog = p("MATCH (n) RETURN n.x * n.y");
        match &ret_expr(&prog).kind {
            ExprKind::BinaryOp { op, .. } => assert_eq!(*op, BinaryOp::Mul),
            other => panic!("expected BinaryOp, got {other:?}"),
        }
    }

    #[test]
    fn div() {
        let prog = p("MATCH (n) RETURN n.x / n.y");
        match &ret_expr(&prog).kind {
            ExprKind::BinaryOp { op, .. } => assert_eq!(*op, BinaryOp::Div),
            other => panic!("expected BinaryOp, got {other:?}"),
        }
    }

    #[test]
    fn unary_neg() {
        let prog = p("MATCH (n) RETURN -n.x");
        match &ret_expr(&prog).kind {
            ExprKind::UnaryOp { op, expr } => {
                assert_eq!(*op, UnaryOp::Neg);
                assert!(
                    matches!(&expr.as_ref().kind, ExprKind::PropertyAccess { property, .. } if property == "x")
                );
            }
            other => panic!("expected UnaryOp, got {other:?}"),
        }
    }

    #[test]
    fn unary_pos() {
        let prog = p("MATCH (n) RETURN +n.x");
        match &ret_expr(&prog).kind {
            ExprKind::UnaryOp { op, .. } => assert_eq!(*op, UnaryOp::Pos),
            other => panic!("expected UnaryOp, got {other:?}"),
        }
    }
}
