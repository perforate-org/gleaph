//! §14.1 — Composite query statement.
//!
//! GQL rules: compositeQueryStatement.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── compositeQueryStatement ──────────────────────────────────────────────
//   : compositeQueryExpression
//   ;
mod composite_query_statement {
    use super::*;

    /// A bare `MATCH (n) RETURN n` produces Statement::Query(CompositeQueryExpr).
    #[test]
    fn bare_match_return_is_query() {
        let prog = p("MATCH (n) RETURN n");
        let b = body(&prog);
        assert!(
            matches!(&b.first, Statement::Query(_)),
            "expected Statement::Query, got {:?}",
            b.first
        );
    }

    /// The inner CompositeQueryExpr has an empty `rest` for a single query.
    #[test]
    fn single_query_rest_is_empty() {
        let prog = p("MATCH (n) RETURN n");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert!(q.rest.is_empty(), "expected empty rest for single query");
        } else {
            panic!("expected Statement::Query");
        }
    }
}
