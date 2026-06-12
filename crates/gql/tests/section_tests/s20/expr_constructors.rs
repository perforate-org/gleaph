//! §20.14 — Constructed values (list, path, slice).

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

#[cfg(feature = "cypher")]
mod list_slice {
    use super::*;

    // ── List slicing ────────────────────────────────────────────────────────

    #[cfg(feature = "cypher")]
    #[test]
    fn slice_to_only() {
        // expr[..to] — lines 91-98
        let prog = p("MATCH (n) RETURN n.items[..3]");
        match &ret_expr(&prog).kind {
            ExprKind::ListSlice { from, to, .. } => {
                assert!(from.is_none());
                assert!(to.is_some());
            }
            other => panic!("expected ListSlice, got {other:?}"),
        }
    }

    #[test]
    fn slice_from_to() {
        // expr[from..to] — line 105-106
        let prog = p("MATCH (n) RETURN n.items[1..3]");
        match &ret_expr(&prog).kind {
            ExprKind::ListSlice { from, to, .. } => {
                assert!(from.is_some());
                assert!(to.is_some());
            }
            other => panic!("expected ListSlice, got {other:?}"),
        }
    }
}

// ── LIST/ARRAY constructors ─────────────────────────────────────────────

#[test]
fn empty_list_constructor() {
    // Line 494 (empty list)
    let prog = p("MATCH (n) RETURN LIST[]");
    match &ret_expr(&prog).kind {
        ExprKind::ListConstructor { items, .. } => {
            assert!(items.is_empty());
        }
        other => panic!("expected ListConstructor, got {other:?}"),
    }
}

// ── PATH constructor ────────────────────────────────────────────────────

#[test]
fn path_constructor_three_elems() {
    // Lines 514, PATH[...] with odd elements
    let prog = p("MATCH (n) RETURN PATH[n, e, m]");
    match &ret_expr(&prog).kind {
        ExprKind::PathConstructor { elements } => {
            assert_eq!(elements.len(), 3);
        }
        other => panic!("expected PathConstructor, got {other:?}"),
    }
}

#[test]
fn path_constructor_error_even() {
    // Lines 519-522 (error: even number of elements)
    let result = gleaph_gql::parser::parse("MATCH (n) RETURN PATH[n, e]");
    assert!(result.is_err());
}

// ── PATH_LENGTH ─────────────────────────────────────────────────────────

#[test]
fn path_length_func() {
    // Lines 589-593
    let prog = p("MATCH (n) RETURN PATH_LENGTH(n.p)");
    match &ret_expr(&prog).kind {
        ExprKind::PathLength(_) => {}
        other => panic!("expected PathLength, got {other:?}"),
    }
}
