//! §16.14 — YIELD clause.
//!
//! GQL rule: `graphPatternYieldClause : YIELD graphPatternYieldItemList`

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

/// Helper to extract the MatchStatement from a query string.
fn ms(input: &str) -> MatchStatement {
    let prog = p(input);
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            for part in &cq.left.parts {
                if let SimpleQueryStatement::Match(m) = part {
                    return m.clone();
                }
            }
            panic!("no Match found in parts: {:?}", cq.left.parts);
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

// ── graphPatternYieldClause ─────────────────────────────────────────────
mod yield_clause {
    use super::*;

    /// MATCH (n) YIELD n RETURN n — yield_items is Some with 1 item
    #[test]
    fn yield_on_match() {
        let m = ms("MATCH (n) YIELD n RETURN n");
        let items = m
            .yield_items
            .as_ref()
            .expect("expected yield_items to be Some");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "n");
    }

    /// MATCH (a)-[e]->(b) YIELD a, b RETURN a — yield_items has 2 items
    #[test]
    fn yield_multiple() {
        let m = ms("MATCH (a)-[e]->(b) YIELD a, b RETURN a");
        let items = m
            .yield_items
            .as_ref()
            .expect("expected yield_items to be Some");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "a");
        assert_eq!(items[1].name, "b");
    }

    /// MATCH (n) RETURN n — no YIELD, yield_items is None
    #[test]
    fn no_yield() {
        let m = ms("MATCH (n) RETURN n");
        assert!(
            m.yield_items.is_none(),
            "expected yield_items to be None when no YIELD clause"
        );
    }
}
