//! §16.17 — Sort specification.
//!
//! GQL rule: `sortSpecification : sortKey orderingSpecification? nullOrdering?`

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── sortSpecification ───────────────────────────────────────────────────
mod sort_specification {
    use super::*;

    /// MATCH (n) RETURN n ORDER BY n.name ASC NULLS FIRST
    /// → null_order is Some(NullOrder::First)
    #[test]
    fn nulls_first() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.name ASC NULLS FIRST");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let result = cq.left.result.as_ref().expect("expected result statement");
                match result {
                    ResultStatement::Return(ret) => match &ret.body {
                        ReturnBody::Items { order_by, .. } => {
                            let ob = order_by.as_ref().expect("expected order_by to be Some");
                            assert_eq!(
                                ob.items[0].null_order,
                                Some(NullOrder::First),
                                "expected NULLS FIRST"
                            );
                        }
                        other => panic!("expected ReturnBody::Items, got {other:?}"),
                    },
                    other => panic!("expected Return, got {other:?}"),
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    /// MATCH (n) RETURN n ORDER BY n.name DESC NULLS LAST
    /// → null_order is Some(NullOrder::Last)
    #[test]
    fn nulls_last() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.name DESC NULLS LAST");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let result = cq.left.result.as_ref().expect("expected result statement");
                match result {
                    ResultStatement::Return(ret) => match &ret.body {
                        ReturnBody::Items { order_by, .. } => {
                            let ob = order_by.as_ref().expect("expected order_by to be Some");
                            assert_eq!(
                                ob.items[0].null_order,
                                Some(NullOrder::Last),
                                "expected NULLS LAST"
                            );
                        }
                        other => panic!("expected ReturnBody::Items, got {other:?}"),
                    },
                    other => panic!("expected Return, got {other:?}"),
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}
