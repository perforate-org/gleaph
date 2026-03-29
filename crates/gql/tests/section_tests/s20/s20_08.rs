//! §20.8 — CAST specification.
//!
//! GQL rules: castSpecification.

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

// ── castSpecification ───────────────────────────────────────────────────
mod cast_specification {
    use super::*;

    /// CAST(n.x AS STRING)
    #[test]
    fn cast_to_string() {
        let prog = p("MATCH (n) RETURN CAST(n.x AS STRING)");
        match &ret_expr(&prog).kind {
            ExprKind::Cast { expr, target } => {
                assert!(
                    matches!(&expr.as_ref().kind, ExprKind::PropertyAccess { property, .. } if property == "x")
                );
                assert_eq!(
                    *target,
                    ValueType::String {
                        min_length: None,
                        max_length: None
                    }
                );
            }
            other => panic!("expected Cast, got {other:?}"),
        }
    }

    /// CAST(n.x AS LIST<INT32>)
    #[test]
    fn cast_to_list() {
        let prog = p("MATCH (n) RETURN CAST(n.x AS LIST<INT32>)");
        match &ret_expr(&prog).kind {
            ExprKind::Cast { target, .. } => match target {
                ValueType::List {
                    element_type,
                    max_length,
                    ..
                } => {
                    assert!(matches!(**element_type, ValueType::Int32 { .. }));
                    assert_eq!(*max_length, None);
                }
                other => panic!("expected ValueType::List, got {other:?}"),
            },
            other => panic!("expected Cast, got {other:?}"),
        }
    }
}
