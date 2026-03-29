//! §20.10–§20.11 — Element ID and property reference.
//!
//! GQL rules: elementIdFunction, propertyReference.

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

// ── elementIdFunction ───────────────────────────────────────────────────
mod element_id_function {
    use super::*;

    /// ELEMENT_ID(n) — dedicated AST variant
    #[test]
    fn element_id() {
        let prog = p("MATCH (n) RETURN ELEMENT_ID(n)");
        match &ret_expr(&prog).kind {
            ExprKind::ElementId(inner) => {
                assert_eq!(*inner.as_ref(), Expr::var("n"));
            }
            other => panic!("expected ElementId, got {other:?}"),
        }
    }
}

// ── propertyReference ───────────────────────────────────────────────────
mod property_reference {
    use super::*;

    /// n.name — simple property access
    #[test]
    fn simple_property() {
        let prog = p("MATCH (n) RETURN n.name");
        match &ret_expr(&prog).kind {
            ExprKind::PropertyAccess { expr, property } => {
                assert_eq!(*expr.as_ref(), Expr::var("n"));
                assert_eq!(property, "name");
            }
            other => panic!("expected PropertyAccess, got {other:?}"),
        }
    }

    /// n.address.city — nested property access
    #[test]
    fn nested_property() {
        let prog = p("MATCH (n) RETURN n.address.city");
        match &ret_expr(&prog).kind {
            ExprKind::PropertyAccess {
                expr: outer,
                property: city,
            } => {
                assert_eq!(city, "city");
                match &outer.as_ref().kind {
                    ExprKind::PropertyAccess {
                        expr: inner,
                        property: address,
                    } => {
                        assert_eq!(address, "address");
                        assert_eq!(*inner.as_ref(), Expr::var("n"));
                    }
                    other => panic!("expected nested PropertyAccess, got {other:?}"),
                }
            }
            other => panic!("expected PropertyAccess, got {other:?}"),
        }
    }
}
