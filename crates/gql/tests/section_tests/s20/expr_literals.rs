//! §20.3 — Literals and label predicates.

use crate::section_tests::p;
use gleaph_gql::Value;
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

// ── Colon label predicate ───────────────────────────────────────────────

#[test]
fn colon_label_expr() {
    // expr :Label — lines 231-238
    let prog = p("MATCH (n) RETURN n :Person");
    match &ret_expr(&prog).kind {
        ExprKind::IsLabeled { negated, .. } => {
            assert!(!negated);
        }
        other => panic!("expected IsLabeled, got {other:?}"),
    }
}

// ── BigInt / ExactNumeric literals ──────────────────────────────────────

#[test]
fn bigint_i128() {
    // Lines 382-385, 785
    let prog = p("MATCH (n) RETURN 99999999999999999999");
    match &ret_expr(&prog).kind {
        ExprKind::Literal(Value::Int128(_)) => {}
        other => panic!("expected Int128, got {other:?}"),
    }
}

#[test]
fn bigint_u128() {
    // Line 789
    let prog = p("MATCH (n) RETURN 200000000000000000000000000000000000000");
    match &ret_expr(&prog).kind {
        ExprKind::Literal(Value::Uint128(_)) => {}
        other => panic!("expected Uint128, got {other:?}"),
    }
}

#[test]
fn exact_numeric() {
    // Lines 396-401
    let prog = p("MATCH (n) RETURN 3.14M");
    match &ret_expr(&prog).kind {
        ExprKind::Literal(Value::Decimal(_)) => {}
        other => panic!("expected Decimal, got {other:?}"),
    }
}

// ── UNKNOWN / NULL literals ─────────────────────────────────────────────

#[test]
fn unknown_literal() {
    // Lines 479-480
    let prog = p("MATCH (n) RETURN UNKNOWN");
    match &ret_expr(&prog).kind {
        ExprKind::Literal(Value::Null) => {}
        other => panic!("expected Null, got {other:?}"),
    }
}
