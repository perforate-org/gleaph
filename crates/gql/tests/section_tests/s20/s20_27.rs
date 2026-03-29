//! §20.27–§20.29 — Datetime functions.
//!
//! GQL rules: currentDateFunction, currentTimeFunction,
//! currentTimestampFunction, dateLiteral, dateFunction, durationLiteral,
//! durationFunction, durationBetweenFunction.

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

// ── current datetime functions ──────────────────────────────────────────
mod current_datetime_functions {
    use super::*;

    /// CURRENT_DATE
    #[test]
    fn current_date() {
        let prog = p("MATCH (n) RETURN CURRENT_DATE");
        match &ret_expr(&prog).kind {
            ExprKind::CurrentDate => {}
            other => panic!("expected CurrentDate, got {other:?}"),
        }
    }

    /// CURRENT_TIME
    #[test]
    fn current_time() {
        let prog = p("MATCH (n) RETURN CURRENT_TIME");
        match &ret_expr(&prog).kind {
            ExprKind::CurrentTime => {}
            other => panic!("expected CurrentTime, got {other:?}"),
        }
    }

    /// CURRENT_TIMESTAMP
    #[test]
    fn current_timestamp() {
        let prog = p("MATCH (n) RETURN CURRENT_TIMESTAMP");
        match &ret_expr(&prog).kind {
            ExprKind::CurrentTimestamp => {}
            other => panic!("expected CurrentTimestamp, got {other:?}"),
        }
    }

    /// LOCAL_TIME
    #[test]
    fn local_time() {
        let prog = p("MATCH (n) RETURN LOCAL_TIME");
        match &ret_expr(&prog).kind {
            ExprKind::CurrentLocalTime => {}
            other => panic!("expected CurrentLocalTime, got {other:?}"),
        }
    }

    /// LOCAL_TIMESTAMP
    #[test]
    fn local_timestamp() {
        let prog = p("MATCH (n) RETURN LOCAL_TIMESTAMP");
        match &ret_expr(&prog).kind {
            ExprKind::CurrentLocalTimestamp => {}
            other => panic!("expected CurrentLocalTimestamp, got {other:?}"),
        }
    }
}

// ── date/datetime constructors ──────────────────────────────────────────
mod datetime_constructors {
    use super::*;

    /// DATE '2024-01-15' — date literal
    #[test]
    fn date_literal() {
        let prog = p("MATCH (n) RETURN DATE '2024-01-15'");
        match &ret_expr(&prog).kind {
            ExprKind::DateLiteral(args) => {
                assert!(!args.is_empty());
            }
            other => panic!("expected DateLiteral, got {other:?}"),
        }
    }

    /// DATE(2024, 1, 15) — date function
    #[test]
    fn date_function() {
        let prog = p("MATCH (n) RETURN DATE(2024, 1, 15)");
        match &ret_expr(&prog).kind {
            ExprKind::DateFunction(args) => {
                assert_eq!(args.len(), 3);
                assert_eq!(args[0], Expr::int(2024));
                assert_eq!(args[1], Expr::int(1));
                assert_eq!(args[2], Expr::int(15));
            }
            other => panic!("expected DateFunction, got {other:?}"),
        }
    }

    /// DURATION 'P1Y2M' — duration literal
    #[test]
    fn duration_literal() {
        let prog = p("MATCH (n) RETURN DURATION 'P1Y2M'");
        match &ret_expr(&prog).kind {
            ExprKind::DurationLiteral(args) => {
                assert!(!args.is_empty());
            }
            other => panic!("expected DurationLiteral, got {other:?}"),
        }
    }

    /// DURATION_BETWEEN(n.start, n.end) — duration between function
    #[test]
    fn duration_between() {
        let prog = p("MATCH (n) RETURN DURATION_BETWEEN(n.start, n.end)");
        match &ret_expr(&prog).kind {
            ExprKind::DurationBetween { left, right, .. } => {
                assert!(
                    matches!(&left.as_ref().kind, ExprKind::PropertyAccess { property, .. } if property == "start")
                );
                assert!(
                    matches!(&right.as_ref().kind, ExprKind::PropertyAccess { property, .. } if property == "end")
                );
            }
            other => panic!("expected DurationBetween, got {other:?}"),
        }
    }
}
