//! §20.4 — Dynamic parameter specification.
//!
//! GQL rules: dynamicParameterSpecification.

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

// ── dynamicParameterSpecification ───────────────────────────────────────
mod dynamic_parameter_specification {
    use super::*;

    /// $param — a named parameter reference
    #[test]
    fn named_parameter() {
        let prog = p("MATCH (n) RETURN $param");
        match &ret_expr(&prog).kind {
            ExprKind::Parameter(name) => {
                // Parameter name includes the $ prefix
                assert_eq!(name, "$param");
            }
            other => panic!("expected Parameter, got {other:?}"),
        }
    }
}
