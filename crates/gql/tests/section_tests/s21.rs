//! §21 — Literals and tokens.
//!
//! GQL rules: unsignedIntegerLiteral, unsignedDecimalInExactNumericLiteral,
//! characterStringLiteral, byteStringLiteral, identifier, quotedIdentifier.

use crate::section_tests::{body, p};
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

// ── unsignedIntegerLiteral ──────────────────────────────────────────────
//   §21.2 — Integer and numeric literals
mod unsigned_integer {
    use super::*;

    /// Zero: 0 → Value::Int64(0)
    #[test]
    fn zero() {
        let prog = p("MATCH (n) WHERE n.x = 0 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Compare { right, .. } => {
                assert_eq!(
                    *right.as_ref(),
                    Expr::new(ExprKind::Literal(Value::Int64(0)))
                );
            }
            other => panic!("expected Compare, got {other:?}"),
        }
    }

    /// Negative: -42 → UnaryOp { op: Neg, expr: Literal(Int64(42)) }
    #[test]
    fn negative() {
        let prog = p("MATCH (n) RETURN -42");
        match &ret_expr(&prog).kind {
            ExprKind::UnaryOp { op, expr } => {
                assert_eq!(*op, UnaryOp::Neg);
                assert_eq!(
                    *expr.as_ref(),
                    Expr::new(ExprKind::Literal(Value::Int64(42)))
                );
            }
            other => panic!("expected UnaryOp(Neg), got {other:?}"),
        }
    }

    /// Scientific float: 1.5e10 → Value::Float64(_)
    #[test]
    fn scientific_float() {
        let prog = p("MATCH (n) WHERE n.x = 1.5e10 RETURN n");
        match &where_expr(&prog).kind {
            ExprKind::Compare { right, .. } => match &right.as_ref().kind {
                ExprKind::Literal(Value::Float64(v)) => {
                    assert!((v - 1.5e10).abs() < 1.0);
                }
                other => panic!("expected Literal(Float64), got {other:?}"),
            },
            other => panic!("expected Compare, got {other:?}"),
        }
    }
}

// ── quotedIdentifier ────────────────────────────────────────────────────
//   §21.3 — Quoted identifiers (backtick)
mod quoted_identifier {
    use super::*;

    /// MATCH (`node with spaces`) — variable name is "node with spaces"
    #[test]
    fn backtick_identifier() {
        let prog = p("MATCH (`node with spaces`) RETURN `node with spaces`");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                // Check the RETURN expression is a variable with the quoted name
                match cq.left.result.as_ref().unwrap() {
                    ResultStatement::Return(ret) => match &ret.body {
                        ReturnBody::Items { items, .. } => match &items[0].expr.kind {
                            ExprKind::Variable(name) => {
                                assert_eq!(name, "node with spaces");
                            }
                            other => panic!("expected Variable, got {other:?}"),
                        },
                        other => panic!("expected Items, got {other:?}"),
                    },
                    other => panic!("expected Return, got {other:?}"),
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}
