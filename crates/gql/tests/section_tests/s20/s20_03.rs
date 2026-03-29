//! §20.3 — Value specification (literals).
//!
//! GQL rules: unsignedValueSpecification, generalLiteral.

use crate::section_tests::p;
use gleaph_gql::Value;
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

// ── literals ────────────────────────────────────────────────────────────
mod literals {
    use super::*;

    #[test]
    fn integer_literal() {
        let prog = p("MATCH (n) WHERE n.x = 42 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Compare { right, .. } => {
                assert_eq!(
                    *right.as_ref(),
                    Expr::new(ExprKind::Literal(Value::Int64(42)))
                );
            }
            other => panic!("expected Compare, got {other:?}"),
        }
    }

    #[test]
    fn float_literal() {
        let prog = p("MATCH (n) WHERE n.x = 3.14 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Compare { right, .. } => match &right.as_ref().kind {
                ExprKind::Literal(Value::Float64(v)) => {
                    #[allow(clippy::approx_constant)]
                    let pi_approx = 3.14;
                    assert!((v - pi_approx).abs() < 1e-10);
                }
                other => panic!("expected Literal(Float64), got {other:?}"),
            },
            other => panic!("expected Compare, got {other:?}"),
        }
    }

    #[test]
    fn string_literal() {
        let prog = p("MATCH (n) WHERE n.name = 'hello' RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Compare { right, .. } => {
                assert_eq!(
                    *right.as_ref(),
                    Expr::new(ExprKind::Literal(Value::Text("hello".into())))
                );
            }
            other => panic!("expected Compare, got {other:?}"),
        }
    }

    #[test]
    fn boolean_true() {
        let prog = p("MATCH (n) RETURN TRUE");
        match &ret_expr(&prog).kind {
            ExprKind::Literal(Value::Bool(true)) => {}
            other => panic!("expected Literal(Bool(true)), got {other:?}"),
        }
    }

    #[test]
    fn boolean_false() {
        let prog = p("MATCH (n) RETURN FALSE");
        match &ret_expr(&prog).kind {
            ExprKind::Literal(Value::Bool(false)) => {}
            other => panic!("expected Literal(Bool(false)), got {other:?}"),
        }
    }

    #[test]
    fn null_literal() {
        let prog = p("MATCH (n) RETURN NULL");
        match &ret_expr(&prog).kind {
            ExprKind::Literal(Value::Null) => {}
            other => panic!("expected Literal(Null), got {other:?}"),
        }
    }
}
