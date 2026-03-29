//! §16.19 — OFFSET clause.
//!
//! GQL rule: `offsetClause : OFFSET unsignedIntegerSpecification`

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── offsetClause ────────────────────────────────────────────────────────
mod offset_clause {
    use super::*;

    /// MATCH (n) RETURN n OFFSET 5 — offset is Some with count == 5
    #[test]
    fn offset_present() {
        let prog = p("MATCH (n) RETURN n OFFSET 5");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let result = cq.left.result.as_ref().expect("expected result statement");
                match result {
                    ResultStatement::Return(ret) => match &ret.body {
                        ReturnBody::Items { offset, .. } => {
                            let off = offset.as_ref().expect("expected offset to be Some");
                            assert_eq!(off.count, Expr::int(5), "expected OFFSET 5");
                        }
                        other => panic!("expected ReturnBody::Items, got {other:?}"),
                    },
                    other => panic!("expected Return, got {other:?}"),
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    /// MATCH (n) RETURN n — no offset
    #[test]
    fn no_offset() {
        let prog = p("MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let result = cq.left.result.as_ref().expect("expected result statement");
                match result {
                    ResultStatement::Return(ret) => match &ret.body {
                        ReturnBody::Items { offset, .. } => {
                            assert!(offset.is_none(), "expected offset to be None");
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}
