//! §20.5 — LET value expression.
//!
//! GQL rules: letValueExpression.

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

// ── letValueExpression ──────────────────────────────────────────────────
mod let_value_expression {
    use super::*;

    /// LET x = n.age IN x * 2 END — Expr::LetIn with one binding
    #[test]
    fn let_in_single_binding() {
        let prog = p("MATCH (n) RETURN LET x = n.age IN x * 2 END");
        match &ret_expr(&prog).kind {
            ExprKind::LetIn { bindings, expr } => {
                assert_eq!(bindings.len(), 1);
                assert_eq!(bindings[0].variable, "x");
                assert!(matches!(
                    &bindings[0].value.kind,
                    ExprKind::PropertyAccess { property, .. } if property == "age"
                ));
                // The body should be x * 2
                match &expr.as_ref().kind {
                    ExprKind::BinaryOp { op, .. } => assert_eq!(*op, BinaryOp::Mul),
                    other => panic!("expected BinaryOp in body, got {other:?}"),
                }
            }
            other => panic!("expected LetIn, got {other:?}"),
        }
    }
}
