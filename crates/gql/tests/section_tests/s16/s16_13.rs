//! §16.13 — WHERE clause.
//!
//! GQL rule: `graphPatternWhereClause : WHERE searchCondition`

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

// ── graphPatternWhereClause ─────────────────────────────────────────────
mod where_clause {
    use super::*;

    /// MATCH (n) WHERE n.age > 30 RETURN n — pattern.where_clause is Some
    #[test]
    fn where_in_match() {
        let m = ms("MATCH (n) WHERE n.age > 30 RETURN n");
        assert!(
            m.pattern.where_clause.is_some(),
            "expected pattern.where_clause to be Some"
        );
    }

    /// MATCH (n WHERE n.age > 30) RETURN n — NodePattern.where_clause is Some
    #[test]
    fn where_in_node_pattern() {
        let m = ms("MATCH (n WHERE n.age > 30) RETURN n");
        if let PathPatternExpr::Term(t) = &m.pattern.paths[0].expr
            && let PathPrimary::Node(np) = &t.factors[0].primary
        {
            assert!(
                np.where_clause.is_some(),
                "expected node where_clause to be Some"
            );
            return;
        }
        panic!("could not find node pattern in first path");
    }
}
