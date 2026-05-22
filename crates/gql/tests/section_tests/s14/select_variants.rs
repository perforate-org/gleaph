//! §14.12 — SELECT statement variants.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── SELECT variants ─────────────────────────────────────────────────────

#[test]
fn select_all() {
    let prog = p("SELECT ALL n.name FROM myGraph MATCH (n)");
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => match cq.left.result.as_ref().unwrap() {
            ResultStatement::Select(sel) => {
                assert_eq!(sel.set_quantifier, SetQuantifier::All);
            }
            other => panic!("expected Select, got {other:?}"),
        },
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn select_without_from() {
    let prog = p("SELECT 1 + 2");
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => match cq.left.result.as_ref().unwrap() {
            ResultStatement::Select(sel) => {
                assert!(sel.source.is_none());
            }
            other => panic!("expected Select, got {other:?}"),
        },
        other => panic!("expected Query, got {other:?}"),
    }
}
