//! §20.6 — Value query expression.
//!
//! GQL rules: valueQueryExpression.

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

// ── valueQueryExpression ────────────────────────────────────────────────
mod value_query_expression {
    use super::*;

    /// VALUE { MATCH (m) RETURN m.age } — value subquery
    #[test]
    fn value_subquery() {
        let prog = p("MATCH (n) RETURN VALUE { MATCH (m) RETURN m.age }");
        match &ret_expr(&prog).kind {
            ExprKind::ValueSubquery(cq) => {
                // The inner query should have a result statement
                assert!(cq.left.result.is_some());
            }
            other => panic!("expected ValueSubquery, got {other:?}"),
        }
    }
}
