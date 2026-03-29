//! §16.16 — ORDER BY clause.
//!
//! GQL rule: `orderByClause : ORDER BY sortSpecificationList`

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── orderByClause ───────────────────────────────────────────────────────
mod order_by_clause {
    use super::*;

    /// MATCH (n) RETURN n ORDER BY n.name ASC, n.age DESC
    /// → order_by is Some with 2 sort items
    #[test]
    fn multiple_sorts() {
        let prog = p("MATCH (n) RETURN n ORDER BY n.name ASC, n.age DESC");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let result = cq.left.result.as_ref().expect("expected result statement");
                match result {
                    ResultStatement::Return(ret) => match &ret.body {
                        ReturnBody::Items { order_by, .. } => {
                            let ob = order_by.as_ref().expect("expected order_by to be Some");
                            assert_eq!(ob.items.len(), 2, "expected 2 sort items");
                            assert_eq!(
                                ob.items[0].direction,
                                Some(SortDirection::Asc),
                                "expected first sort ASC"
                            );
                            assert_eq!(
                                ob.items[1].direction,
                                Some(SortDirection::Desc),
                                "expected second sort DESC"
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
