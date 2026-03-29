//! §14.4 — Match statement.
//!
//! GQL rules: matchStatement, simpleMatchStatement, optionalMatchStatement,
//! optionalOperand, graphPatternBindingTable.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── matchStatement ───────────────────────────────────────────────────────
//   : simpleMatchStatement | optionalMatchStatement
//   ;
// ── simpleMatchStatement ─────────────────────────────────────────────────
//   : MATCH graphPatternBindingTable
//   ;
// ── optionalMatchStatement ───────────────────────────────────────────────
//   : OPTIONAL optionalOperand
//   ;
// ── graphPatternBindingTable ─────────────────────────────────────────────
//   : graphPattern graphPatternYieldClause?
//   ;
mod match_statement {
    use super::*;

    fn extract_match(input: &str) -> MatchStatement {
        let prog = p(input);
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            for part in &q.left.parts {
                if let SimpleQueryStatement::Match(m) = part {
                    return m.clone();
                }
            }
            panic!("no Match found in parts: {:?}", q.left.parts);
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// MATCH (n) RETURN n — optional=false
    #[test]
    fn simple_match_not_optional() {
        let m = extract_match("MATCH (n) RETURN n");
        assert!(!m.optional, "expected optional=false");
    }

    /// OPTIONAL MATCH (n) RETURN n — optional=true
    #[test]
    fn optional_match() {
        let m = extract_match("OPTIONAL MATCH (n) RETURN n");
        assert!(m.optional, "expected optional=true");
    }

    /// MATCH (n) — graph_name is None for standalone.
    #[test]
    fn no_graph_name() {
        let m = extract_match("MATCH (n) RETURN n");
        assert!(m.graph_name.is_none(), "expected no graph_name");
    }

    /// MATCH pattern has at least one path.
    #[test]
    fn pattern_has_paths() {
        let m = extract_match("MATCH (n) RETURN n");
        assert!(
            !m.pattern.paths.is_empty(),
            "expected at least one path pattern"
        );
    }

    /// MATCH (n) YIELD n RETURN n — yield_items is Some.
    #[test]
    fn match_with_yield() {
        let m = extract_match("MATCH (n) YIELD n RETURN n");
        assert!(
            m.yield_items.is_some(),
            "expected yield_items to be Some, got None"
        );
        let items = m.yield_items.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "n");
    }
}
