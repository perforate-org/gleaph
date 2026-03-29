//! §14.3 — Linear query statement.
//!
//! GQL rules: linearQueryStatement, ambientLinearQueryStatement,
//! simpleLinearQueryStatement, simpleQueryStatement, primitiveQueryStatement.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── linearQueryStatement ─────────────────────────────────────────────────
//   : focusedLinearQueryStatement | ambientLinearQueryStatement
//   ;
// ── ambientLinearQueryStatement ──────────────────────────────────────────
//   : simpleLinearQueryStatement? primitiveResultStatement
//   | nestedQuerySpecification
//   ;
// ── simpleLinearQueryStatement ───────────────────────────────────────────
//   : simpleQueryStatement+
//   ;
// ── simpleQueryStatement ─────────────────────────────────────────────────
//   : primitiveQueryStatement | callQueryStatement
//   ;
// ── primitiveQueryStatement ──────────────────────────────────────────────
//   : matchStatement | letStatement | forStatement | filterStatement
//   | orderByAndPageStatement
//   ;
mod linear_query_statement {
    use super::*;

    /// Ambient with MATCH + RETURN: parts has Match, result has Return.
    #[test]
    fn ambient_match_return() {
        let prog = p("MATCH (n) RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert_eq!(q.left.parts.len(), 1);
            assert!(
                matches!(&q.left.parts[0], SimpleQueryStatement::Match(_)),
                "expected Match, got {:?}",
                q.left.parts[0]
            );
            assert!(
                matches!(&q.left.result, Some(ResultStatement::Return(_))),
                "expected Return result"
            );
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// Focused: `USE g MATCH (n) RETURN n` — parts has Focused.
    #[test]
    fn focused_use_graph() {
        let prog = p("USE g MATCH (n) RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let has_focused = q
                .left
                .parts
                .iter()
                .any(|p| matches!(p, SimpleQueryStatement::Focused { .. }));
            assert!(has_focused, "expected Focused in parts: {:?}", q.left.parts);
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// Multiple simple queries: `MATCH (n) MATCH (m) RETURN n, m` — parts has 2 Match entries.
    #[test]
    fn multiple_match_statements() {
        let prog = p("MATCH (n) MATCH (m) RETURN n, m");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let match_count = q
                .left
                .parts
                .iter()
                .filter(|p| matches!(p, SimpleQueryStatement::Match(_)))
                .count();
            assert_eq!(match_count, 2, "expected 2 Match parts, got {match_count}");
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// Result is None when there is no RETURN/SELECT (data modification only).
    #[test]
    fn no_result_statement() {
        let prog = p("MATCH (n) DELETE n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert!(
                q.left.result.is_none(),
                "expected no result statement, got {:?}",
                q.left.result
            );
        } else {
            panic!("expected Statement::Query");
        }
    }
}
