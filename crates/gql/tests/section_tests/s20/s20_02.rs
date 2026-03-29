//! §20.2 — Value expression primary.
//!
//! GQL rules: valueExpressionPrimary, parenthesizedValueExpression,
//! nonParenthesizedValueExpressionPrimary.

use crate::section_tests::p;
use gleaph_gql::ast::*;

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

// ── valueExpressionPrimary ──────────────────────────────────────────────
mod value_expression_primary {
    use super::*;

    /// A parenthesized expression `(n.x + 1)` should wrap inner expr in Paren.
    #[test]
    fn parenthesized() {
        let prog = p("MATCH (n) RETURN (n.x + 1)");
        match &ret_expr(&prog).kind {
            ExprKind::Paren(inner) => {
                if let ExprKind::BinaryOp { op, .. } = &inner.as_ref().kind {
                    assert_eq!(*op, BinaryOp::Add);
                } else {
                    panic!("expected BinaryOp inside Paren, got {inner:?}");
                }
            }
            other => panic!("expected Paren, got {other:?}"),
        }
    }

    /// A variable reference `n` is a primary.
    #[test]
    fn variable_ref() {
        let prog = p("MATCH (n) RETURN n");
        match &ret_expr(&prog).kind {
            ExprKind::Variable(name) => assert_eq!(name, "n"),
            other => panic!("expected Variable, got {other:?}"),
        }
    }

    /// Property access `n.name` is a primary.
    #[test]
    fn property_access() {
        let prog = p("MATCH (n) RETURN n.name");
        match &ret_expr(&prog).kind {
            ExprKind::PropertyAccess { expr, property } => {
                assert_eq!(*expr.as_ref(), Expr::var("n"));
                assert_eq!(property, "name");
            }
            other => panic!("expected PropertyAccess, got {other:?}"),
        }
    }
}
