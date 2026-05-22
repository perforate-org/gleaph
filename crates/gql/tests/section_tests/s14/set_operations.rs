//! §14.2 — Composite query set operations.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── Composite query set operations ──────────────────────────────────────

fn set_op(prog: &GqlProgram) -> &SetOp {
    let b = body(prog);
    match &b.first {
        Statement::Query(cq) => {
            assert!(!cq.rest.is_empty(), "expected composite query with rest");
            &cq.rest[0].0
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn union_distinct() {
    let prog = p("MATCH (n) RETURN n.x UNION DISTINCT MATCH (m) RETURN m.x");
    assert_eq!(*set_op(&prog), SetOp::UnionDistinct);
}

#[test]
fn except_all() {
    let prog = p("MATCH (n) RETURN n.x EXCEPT ALL MATCH (m) RETURN m.x");
    assert_eq!(*set_op(&prog), SetOp::ExceptAll);
}

#[test]
fn except_distinct() {
    let prog = p("MATCH (n) RETURN n.x EXCEPT DISTINCT MATCH (m) RETURN m.x");
    assert_eq!(*set_op(&prog), SetOp::ExceptDistinct);
}

#[test]
fn intersect_all() {
    let prog = p("MATCH (n) RETURN n.x INTERSECT ALL MATCH (m) RETURN m.x");
    assert_eq!(*set_op(&prog), SetOp::IntersectAll);
}

#[test]
fn intersect_distinct() {
    let prog = p("MATCH (n) RETURN n.x INTERSECT DISTINCT MATCH (m) RETURN m.x");
    assert_eq!(*set_op(&prog), SetOp::IntersectDistinct);
}

#[test]
fn except_bare() {
    let prog = p("MATCH (n) RETURN n.x EXCEPT MATCH (m) RETURN m.x");
    assert_eq!(*set_op(&prog), SetOp::Except);
}

#[test]
fn intersect_bare() {
    let prog = p("MATCH (n) RETURN n.x INTERSECT MATCH (m) RETURN m.x");
    assert_eq!(*set_op(&prog), SetOp::Intersect);
}
