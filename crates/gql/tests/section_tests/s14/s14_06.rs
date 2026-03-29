//! §14.6 — Filter statement.
//!
//! GQL rules: filterStatement.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── filterStatement ──────────────────────────────────────────────────────
//   : FILTER (whereClause | searchCondition)
//   ;
mod filter_statement {
    use super::*;

    fn extract_filter(input: &str) -> FilterStatement {
        let prog = p(input);
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            for part in &q.left.parts {
                if let SimpleQueryStatement::Filter(f) = part {
                    return f.clone();
                }
            }
            panic!("no Filter found in parts: {:?}", q.left.parts);
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// FILTER WHERE n.age > 30 — with WHERE keyword, condition is comparison.
    #[test]
    fn filter_with_where() {
        let f = extract_filter("MATCH (n) FILTER WHERE n.age > 30 RETURN n");
        assert!(f.where_keyword, "expected where_keyword=true");
        assert!(
            matches!(&f.condition.kind, ExprKind::Compare { .. }),
            "expected Compare expression, got {:?}",
            f.condition
        );
    }

    /// FILTER n.age > 30 — without WHERE keyword.
    #[test]
    fn filter_without_where() {
        let f = extract_filter("MATCH (n) FILTER n.age > 30 RETURN n");
        assert!(!f.where_keyword, "expected where_keyword=false");
        assert!(
            matches!(&f.condition.kind, ExprKind::Compare { .. }),
            "expected Compare expression, got {:?}",
            f.condition
        );
    }

    /// FILTER with boolean expression: FILTER WHERE n.active = TRUE.
    #[test]
    fn filter_boolean_eq() {
        let f = extract_filter("MATCH (n) FILTER WHERE n.active = TRUE RETURN n");
        assert!(
            matches!(&f.condition.kind, ExprKind::Compare { .. }),
            "expected Compare, got {:?}",
            f.condition
        );
    }
}
