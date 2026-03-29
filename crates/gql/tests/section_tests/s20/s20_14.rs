//! §20.14–§20.18 — Constructed values (path, list, record).
//!
//! GQL rules: pathValueConstructor, listValueConstructor,
//! recordConstructor, listLiteral, recordLiteral.

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

// ── pathValueConstructor ────────────────────────────────────────────────
mod path_value_constructor {
    use super::*;

    /// PATH[a, 1, b] — path constructor with 3 elements
    #[test]
    fn path_constructor() {
        let prog = p("MATCH (a), (b) RETURN PATH[a, 1, b]");
        match &ret_expr(&prog).kind {
            ExprKind::PathConstructor { elements } => {
                assert_eq!(elements.len(), 3);
                assert_eq!(elements[0], Expr::var("a"));
                assert_eq!(elements[1], Expr::int(1));
                assert_eq!(elements[2], Expr::var("b"));
            }
            other => panic!("expected PathConstructor, got {other:?}"),
        }
    }
}

// ── listValueConstructor ────────────────────────────────────────────────
mod list_value_constructor {
    use super::*;

    /// [1, 2, 3] — list literal
    #[test]
    fn list_literal() {
        let prog = p("MATCH (n) RETURN [1, 2, 3]");
        match &ret_expr(&prog).kind {
            ExprKind::ListLiteral(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Expr::int(1));
                assert_eq!(items[1], Expr::int(2));
                assert_eq!(items[2], Expr::int(3));
            }
            other => panic!("expected ListLiteral, got {other:?}"),
        }
    }

    /// LIST[1, 2, 3] — keyworded list constructor
    #[test]
    fn list_constructor() {
        let prog = p("MATCH (n) RETURN LIST[1, 2, 3]");
        match &ret_expr(&prog).kind {
            ExprKind::ListConstructor { items, .. } => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Expr::int(1));
                assert_eq!(items[1], Expr::int(2));
                assert_eq!(items[2], Expr::int(3));
            }
            other => panic!("expected ListConstructor, got {other:?}"),
        }
    }

    /// [1, 2] || [3, 4] — concatenation of lists
    #[test]
    fn list_concat() {
        let prog = p("MATCH (n) RETURN [1, 2] || [3, 4]");
        match &ret_expr(&prog).kind {
            ExprKind::Concat(left, right) => {
                assert!(
                    matches!(&left.as_ref().kind, ExprKind::ListLiteral(items) if items.len() == 2)
                );
                assert!(
                    matches!(&right.as_ref().kind, ExprKind::ListLiteral(items) if items.len() == 2)
                );
            }
            other => panic!("expected Concat, got {other:?}"),
        }
    }
}

// ── recordConstructor ───────────────────────────────────────────────────
mod record_constructor {
    use super::*;

    /// {name: 'Alice'} — record literal
    #[test]
    fn record_literal() {
        let prog = p("MATCH (n) RETURN {name: 'Alice'}");
        match &ret_expr(&prog).kind {
            ExprKind::RecordLiteral(fields) => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].0, "name");
                assert_eq!(fields[0].1, Expr::string("Alice"));
            }
            other => panic!("expected RecordLiteral, got {other:?}"),
        }
    }

    /// RECORD {name: 'Alice'} — keyworded record constructor
    #[test]
    fn record_constructor_kw() {
        let prog = p("MATCH (n) RETURN RECORD {name: 'Alice'}");
        match &ret_expr(&prog).kind {
            ExprKind::RecordConstructor(fields) => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].0, "name");
                assert_eq!(fields[0].1, Expr::string("Alice"));
            }
            other => panic!("expected RecordConstructor, got {other:?}"),
        }
    }
}
