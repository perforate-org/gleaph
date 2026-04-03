//! §16.18 — LIMIT clause.
//!
//! GQL rule: `limitClause : LIMIT unsignedIntegerSpecification`

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── limitClause ─────────────────────────────────────────────────────────
mod limit_clause {
    use super::*;

    /// MATCH (n) RETURN n LIMIT 10 — limit is Some with count == 10
    #[test]
    fn limit_present() {
        let prog = p("MATCH (n) RETURN n LIMIT 10");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let result = cq.left.result.as_ref().expect("expected result statement");
                match result {
                    ResultStatement::Return(ret) => match &ret.body {
                        ReturnBody::Items { limit, .. } => {
                            let lim = limit.as_ref().expect("expected limit to be Some");
                            assert_eq!(lim.count, Expr::int(10), "expected LIMIT 10");
                        }
                        other => panic!("expected ReturnBody::Items, got {other:?}"),
                    },
                    other => panic!("expected Return, got {other:?}"),
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    /// MATCH (n) RETURN n — no limit
    #[test]
    fn no_limit() {
        let prog = p("MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let result = cq.left.result.as_ref().expect("expected result statement");
                if let ResultStatement::Return(ret) = result {
                    match &ret.body {
                        ReturnBody::Items { limit, .. } => {
                            assert!(limit.is_none(), "expected limit to be None");
                        }
                        _ => {
                            // Star body has no limit field to check in the same way
                        }
                    }
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}
