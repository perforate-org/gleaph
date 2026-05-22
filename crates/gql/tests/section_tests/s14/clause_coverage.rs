//! §14.9+ — Clause coverage (ORDER BY, LIMIT, RETURN shapes).

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql::validate::validate;

// ── Clause coverage (parser/clause.rs) ──────────────────────────────────

fn return_body(prog: &GqlProgram) -> &ReturnBody {
    let b = body(prog);
    match &b.first {
        Statement::Query(cq) => match cq.left.result.as_ref().unwrap() {
            ResultStatement::Return(ret) => &ret.body,
            other => panic!("expected Return, got {other:?}"),
        },
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn ascending_direction() {
    let prog = p("MATCH (n) RETURN n ORDER BY n.name ASCENDING");
    match return_body(&prog) {
        ReturnBody::Items { order_by, .. } => {
            let order = order_by.as_ref().unwrap();
            assert_eq!(order.items[0].direction, Some(SortDirection::Ascending));
        }
        other => panic!("expected Items, got {other:?}"),
    }
}

#[test]
fn descending_direction() {
    let prog = p("MATCH (n) RETURN n ORDER BY n.name DESCENDING");
    match return_body(&prog) {
        ReturnBody::Items { order_by, .. } => {
            let order = order_by.as_ref().unwrap();
            assert_eq!(order.items[0].direction, Some(SortDirection::Descending));
        }
        other => panic!("expected Items, got {other:?}"),
    }
}

#[test]
fn nulls_first() {
    let prog = p("MATCH (n) RETURN n ORDER BY n.name NULLS FIRST");
    match return_body(&prog) {
        ReturnBody::Items { order_by, .. } => {
            let order = order_by.as_ref().unwrap();
            assert_eq!(order.items[0].null_order, Some(NullOrder::First));
        }
        other => panic!("expected Items, got {other:?}"),
    }
}

#[test]
fn nulls_last() {
    let prog = p("MATCH (n) RETURN n ORDER BY n.name NULLS LAST");
    match return_body(&prog) {
        ReturnBody::Items { order_by, .. } => {
            let order = order_by.as_ref().unwrap();
            assert_eq!(order.items[0].null_order, Some(NullOrder::Last));
        }
        other => panic!("expected Items, got {other:?}"),
    }
}

#[test]
fn empty_group_by() {
    let prog = p("MATCH (n) RETURN COUNT(*) GROUP BY ()");
    match return_body(&prog) {
        ReturnBody::Items { group_by, .. } => {
            let group = group_by.as_ref().unwrap();
            assert!(group.items.is_empty());
        }
        other => panic!("expected Items, got {other:?}"),
    }
}
