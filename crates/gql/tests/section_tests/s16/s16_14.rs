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
        let Some(GraphPatternYield::Items(names)) = m.yield_items.as_ref() else {
            panic!("expected a yield item list");
        };
        assert_eq!(names.as_slice(), ["n"]);
    }

    /// MATCH (a)-[e]->(b) YIELD a, b RETURN a — yield_items has 2 items
    #[test]
    fn yield_multiple() {
        let m = ms("MATCH (a)-[e]->(b) YIELD a, b RETURN a");
        let Some(GraphPatternYield::Items(names)) = m.yield_items.as_ref() else {
            panic!("expected a yield item list");
        };
        assert_eq!(names.as_slice(), ["a", "b"]);
    }

    /// MATCH (n) YIELD NO BINDINGS — the clause projects nothing (16.3).
    #[test]
    fn yield_no_bindings() {
        let m = ms("MATCH (n) YIELD NO BINDINGS RETURN 1");
        assert_eq!(m.yield_items, Some(GraphPatternYield::NoBindings));
    }

    /// 16.3's yield item is a variable reference; 16.14's alias belongs to `CALL`/`NEXT`.
    #[test]
    fn yield_alias_is_rejected_on_match() {
        let error = gleaph_gql::parser::parse("MATCH (n) YIELD n AS m RETURN m")
            .expect_err("MATCH YIELD has no alias");
        assert!(
            format!("{error}").contains("expected"),
            "unexpected message: {error}"
        );
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
