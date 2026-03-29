//! §14.10 — Primitive result statement.
//!
//! GQL rules: primitiveResultStatement.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── primitiveResultStatement ───────────────────────────────────────────
//   : returnStatement orderByAndPageStatement?
//   | FINISH
//   ;
mod primitive_result_statement {
    use super::*;

    /// MATCH (n) RETURN n — result is Return
    #[test]
    fn return_variant() {
        let prog = p("MATCH (n) RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert!(
                matches!(&q.left.result, Some(ResultStatement::Return(_))),
                "expected ResultStatement::Return, got {:?}",
                q.left.result
            );
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// MATCH (n) FINISH — result is Finish
    #[test]
    fn finish_variant() {
        let prog = p("MATCH (n) FINISH");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert!(
                matches!(&q.left.result, Some(ResultStatement::Finish)),
                "expected ResultStatement::Finish, got {:?}",
                q.left.result
            );
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// RETURN with ORDER BY — returnStatement orderByAndPageStatement
    #[test]
    fn return_with_order_by() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.age");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            if let Some(ResultStatement::Return(ret)) = &q.left.result {
                if let ReturnBody::Items { order_by, .. } = &ret.body {
                    assert!(order_by.is_some(), "expected order_by after RETURN");
                } else {
                    panic!("expected ReturnBody::Items");
                }
            } else {
                panic!("expected ResultStatement::Return");
            }
        } else {
            panic!("expected Statement::Query");
        }
    }
}
