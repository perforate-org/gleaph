//! §20.9–20.10 — Aggregates, EXISTS, LABEL.

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

// ── LABEL (singular) ────────────────────────────────────────────────────

#[cfg(feature = "cypher")]
#[test]
fn label_singular() {
    // Lines 631-635
    let prog = p("MATCH (n) RETURN LABEL(n)");
    match &ret_expr(&prog).kind {
        ExprKind::Label(_) => {}
        other => panic!("expected Label, got {other:?}"),
    }
}

// ── EXISTS with parenthesized match ─────────────────────────────────────

#[test]
fn exists_paren_match() {
    // Lines 919-938 — EXISTS(MATCH ...)
    let prog = p("MATCH (n) WHERE EXISTS(MATCH (m)-[:KNOWS]->(n)) RETURN n");
    // Just check it parses correctly
    let _b = crate::section_tests::body(&prog);
}

#[test]
fn exists_brace_match() {
    // Lines 904-905 — EXISTS { MATCH ... RETURN ... }
    let prog = p("MATCH (n) WHERE EXISTS { MATCH (m) RETURN m } RETURN n");
    let _b = crate::section_tests::body(&prog);
}

// ── Aggregate functions ─────────────────────────────────────────────────

#[test]
fn stddev_samp() {
    // Line 1118
    let prog = p("MATCH (n) RETURN STDDEV_SAMP(n.x)");
    match &ret_expr(&prog).kind {
        ExprKind::Aggregate { func, .. } => assert_eq!(*func, AggregateFunc::StddevSamp),
        other => panic!("expected Aggregate, got {other:?}"),
    }
}

#[test]
fn stddev_pop() {
    // Line 1120
    let prog = p("MATCH (n) RETURN STDDEV_POP(n.x)");
    match &ret_expr(&prog).kind {
        ExprKind::Aggregate { func, .. } => assert_eq!(*func, AggregateFunc::StddevPop),
        other => panic!("expected Aggregate, got {other:?}"),
    }
}

#[test]
fn percentile_cont() {
    // Lines 1122, 1140-1141
    let prog = p("MATCH (n) RETURN PERCENTILE_CONT(n.x, 0.5)");
    match &ret_expr(&prog).kind {
        ExprKind::Aggregate { func, expr2, .. } => {
            assert_eq!(*func, AggregateFunc::PercentileCont);
            assert!(expr2.is_some());
        }
        other => panic!("expected Aggregate, got {other:?}"),
    }
}

#[test]
fn percentile_disc() {
    // Line 1124
    let prog = p("MATCH (n) RETURN PERCENTILE_DISC(n.x, 0.9)");
    match &ret_expr(&prog).kind {
        ExprKind::Aggregate { func, .. } => assert_eq!(*func, AggregateFunc::PercentileDisc),
        other => panic!("expected Aggregate, got {other:?}"),
    }
}

#[test]
fn generic_function_no_args() {
    // Line 988 (empty args in generic function)
    let prog = p("MATCH (n) RETURN my_func()");
    match &ret_expr(&prog).kind {
        ExprKind::FunctionCall { args, .. } => assert!(args.is_empty()),
        other => panic!("expected FunctionCall, got {other:?}"),
    }
}
