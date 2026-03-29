//! §14.2 — Composite query expression.
//!
//! GQL rules: compositeQueryExpression, queryConjunction, setOperator.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── compositeQueryExpression ─────────────────────────────────────────────
//   : compositeQueryExpression queryConjunction compositeQueryPrimary
//   | compositeQueryPrimary
//   ;
// ── queryConjunction ─────────────────────────────────────────────────────
//   : setOperator | OTHERWISE
//   ;
// ── setOperator ──────────────────────────────────────────────────────────
//   : UNION setQuantifier? | EXCEPT setQuantifier? | INTERSECT setQuantifier?
//   ;
mod composite_query_expression {
    use super::*;

    /// Single query (no conjunction) — rest is empty.
    #[test]
    fn single_query_no_conjunction() {
        let prog = p("MATCH (n) RETURN n AS x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert!(q.rest.is_empty());
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// UNION — rest[0].0 = SetOp::Union
    #[test]
    fn union() {
        let prog = p("MATCH (n) RETURN n AS x UNION MATCH (m) RETURN m AS x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert_eq!(q.rest.len(), 1);
            assert_eq!(q.rest[0].0, SetOp::Union);
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// UNION ALL — rest[0].0 = SetOp::UnionAll
    #[test]
    fn union_all() {
        let prog = p("MATCH (n) RETURN n AS x UNION ALL MATCH (m) RETURN m AS x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert_eq!(q.rest.len(), 1);
            assert_eq!(q.rest[0].0, SetOp::UnionAll);
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// EXCEPT — rest[0].0 = SetOp::Except
    #[test]
    fn except() {
        let prog = p("MATCH (n) RETURN n AS x EXCEPT MATCH (m) RETURN m AS x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert_eq!(q.rest.len(), 1);
            assert_eq!(q.rest[0].0, SetOp::Except);
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// EXCEPT ALL — rest[0].0 = SetOp::ExceptAll
    #[test]
    fn except_all() {
        let prog = p("MATCH (n) RETURN n AS x EXCEPT ALL MATCH (m) RETURN m AS x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert_eq!(q.rest.len(), 1);
            assert_eq!(q.rest[0].0, SetOp::ExceptAll);
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// INTERSECT — rest[0].0 = SetOp::Intersect
    #[test]
    fn intersect() {
        let prog = p("MATCH (n) RETURN n AS x INTERSECT MATCH (m) RETURN m AS x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert_eq!(q.rest.len(), 1);
            assert_eq!(q.rest[0].0, SetOp::Intersect);
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// INTERSECT ALL — rest[0].0 = SetOp::IntersectAll
    #[test]
    fn intersect_all() {
        let prog = p("MATCH (n) RETURN n AS x INTERSECT ALL MATCH (m) RETURN m AS x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert_eq!(q.rest.len(), 1);
            assert_eq!(q.rest[0].0, SetOp::IntersectAll);
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// OTHERWISE — rest[0].0 = SetOp::Otherwise
    #[test]
    fn otherwise() {
        let prog = p("MATCH (n) RETURN n AS x OTHERWISE MATCH (m) RETURN m AS x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert_eq!(q.rest.len(), 1);
            assert_eq!(q.rest[0].0, SetOp::Otherwise);
        } else {
            panic!("expected Statement::Query");
        }
    }

    /// Multiple conjunctions (3 branches): rest has 2 entries.
    #[test]
    fn multiple_conjunctions() {
        let prog = p("MATCH (a) RETURN a AS x \
             UNION MATCH (b) RETURN b AS x \
             EXCEPT MATCH (c) RETURN c AS x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            assert_eq!(q.rest.len(), 2);
            assert_eq!(q.rest[0].0, SetOp::Union);
            assert_eq!(q.rest[1].0, SetOp::Except);
        } else {
            panic!("expected Statement::Query");
        }
    }
}
