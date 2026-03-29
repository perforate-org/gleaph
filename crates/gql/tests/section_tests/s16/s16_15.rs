//! §16.15 — GROUP BY clause.
//!
//! GQL rule: `groupByClause : GROUP BY groupingElementList`

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── groupByClause ───────────────────────────────────────────────────────
mod group_by_clause {
    use super::*;

    /// MATCH (n) RETURN n.label, COUNT(*) AS cnt GROUP BY n.label
    /// → group_by is Some
    #[test]
    fn group_by_present() {
        let prog = p("MATCH (n) RETURN n.label, COUNT(*) AS cnt GROUP BY n.label");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let result = cq.left.result.as_ref().expect("expected result statement");
                match result {
                    ResultStatement::Return(ret) => match &ret.body {
                        ReturnBody::Items { group_by, .. } => {
                            assert!(group_by.is_some(), "expected group_by to be Some");
                            let gb = group_by.as_ref().unwrap();
                            assert_eq!(gb.items.len(), 1, "expected 1 GROUP BY item");
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
