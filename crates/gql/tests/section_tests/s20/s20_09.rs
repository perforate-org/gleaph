//! §20.9 — Aggregate functions.
//!
//! GQL rules: generalSetFunction, aggregateFunction.

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

// ── aggregateFunction ───────────────────────────────────────────────────
mod aggregate_function {
    use super::*;

    /// COUNT(*) — CountStar, expr is None
    #[test]
    fn count_star() {
        let prog = p("MATCH (n) RETURN COUNT(*)");
        match &ret_expr(&prog).kind {
            ExprKind::Aggregate {
                func,
                expr,
                distinct,
                ..
            } => {
                assert_eq!(*func, AggregateFunc::CountStar);
                assert!(expr.is_none());
                assert!(!distinct);
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    /// COUNT(n) — Count, expr is Some(Variable("n"))
    #[test]
    fn count_expr() {
        let prog = p("MATCH (n) RETURN COUNT(n)");
        match &ret_expr(&prog).kind {
            ExprKind::Aggregate {
                func,
                expr,
                distinct,
                ..
            } => {
                assert_eq!(*func, AggregateFunc::Count);
                assert!(expr.is_some());
                assert_eq!(*expr.as_ref().unwrap().as_ref(), Expr::var("n"));
                assert!(!distinct);
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    /// COUNT(DISTINCT n.label) — distinct is true
    #[test]
    fn count_distinct() {
        let prog = p("MATCH (n) RETURN COUNT(DISTINCT n.label)");
        match &ret_expr(&prog).kind {
            ExprKind::Aggregate {
                func,
                distinct,
                expr,
                ..
            } => {
                assert_eq!(*func, AggregateFunc::Count);
                assert!(distinct);
                assert!(expr.is_some());
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    /// SUM(n.x)
    #[test]
    fn sum() {
        let prog = p("MATCH (n) RETURN SUM(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Aggregate { func, .. } => assert_eq!(*func, AggregateFunc::Sum),
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    /// AVG(n.x)
    #[test]
    fn avg() {
        let prog = p("MATCH (n) RETURN AVG(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Aggregate { func, .. } => assert_eq!(*func, AggregateFunc::Avg),
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    /// MIN(n.x)
    #[test]
    fn min() {
        let prog = p("MATCH (n) RETURN MIN(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Aggregate { func, .. } => assert_eq!(*func, AggregateFunc::Min),
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    /// MAX(n.x)
    #[test]
    fn max() {
        let prog = p("MATCH (n) RETURN MAX(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Aggregate { func, .. } => assert_eq!(*func, AggregateFunc::Max),
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }
}
