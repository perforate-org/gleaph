//! §20.7 — CASE expression.
//!
//! GQL rules: caseExpression, simpleCase, searchedCase, caseAbbreviation.

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

// ── simpleCase ──────────────────────────────────────────────────────────
mod simple_case {
    use super::*;

    /// CASE n.status WHEN 1 THEN 'one' ELSE 'other' END
    #[test]
    fn simple_case_with_else() {
        let prog = p("MATCH (n) RETURN CASE n.status WHEN 1 THEN 'one' ELSE 'other' END");
        match &ret_expr(&prog).kind {
            ExprKind::CaseSimple {
                operand,
                when_clauses,
                else_clause,
            } => {
                assert!(
                    matches!(&operand.as_ref().kind, ExprKind::PropertyAccess { property, .. } if property == "status")
                );
                assert_eq!(when_clauses.len(), 1);
                assert_eq!(when_clauses[0].condition, Expr::int(1));
                assert_eq!(when_clauses[0].result, Expr::string("one"));
                assert!(else_clause.is_some());
                assert_eq!(
                    *else_clause.as_ref().unwrap().as_ref(),
                    Expr::string("other")
                );
            }
            other => panic!("expected CaseSimple, got {other:?}"),
        }
    }
}

// ── searchedCase ────────────────────────────────────────────────────────
mod searched_case {
    use super::*;

    /// CASE WHEN n.x > 0 THEN 'positive' ELSE 'non-positive' END
    #[test]
    fn searched_case_with_else() {
        let prog = p("MATCH (n) RETURN CASE WHEN n.x > 0 THEN 'positive' ELSE 'non-positive' END");
        match &ret_expr(&prog).kind {
            ExprKind::CaseSearched {
                when_clauses,
                else_clause,
            } => {
                assert_eq!(when_clauses.len(), 1);
                match &when_clauses[0].condition.kind {
                    ExprKind::Compare { op, .. } => assert_eq!(*op, CmpOp::Gt),
                    other => panic!("expected Compare in condition, got {other:?}"),
                }
                assert_eq!(when_clauses[0].result, Expr::string("positive"));
                assert!(else_clause.is_some());
            }
            other => panic!("expected CaseSearched, got {other:?}"),
        }
    }
}

// ── caseAbbreviation ────────────────────────────────────────────────────
mod case_abbreviation {
    use super::*;

    /// COALESCE(n.x, 0) — two arguments
    #[test]
    fn coalesce() {
        let prog = p("MATCH (n) RETURN COALESCE(n.x, 0)");
        match &ret_expr(&prog).kind {
            ExprKind::Coalesce(args) => {
                assert_eq!(args.len(), 2);
                assert!(
                    matches!(&args[0].kind, ExprKind::PropertyAccess { property, .. } if property == "x")
                );
                assert_eq!(args[1], Expr::int(0));
            }
            other => panic!("expected Coalesce, got {other:?}"),
        }
    }

    /// NULLIF(n.x, 0)
    #[test]
    fn nullif() {
        let prog = p("MATCH (n) RETURN NULLIF(n.x, 0)");
        match &ret_expr(&prog).kind {
            ExprKind::NullIf(left, right) => {
                assert!(
                    matches!(&left.as_ref().kind, ExprKind::PropertyAccess { property, .. } if property == "x")
                );
                assert_eq!(*right.as_ref(), Expr::int(0));
            }
            other => panic!("expected NullIf, got {other:?}"),
        }
    }
}
