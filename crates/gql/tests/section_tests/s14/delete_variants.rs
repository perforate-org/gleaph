//! §14 — DELETE / DETACH DELETE variants.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── DELETE variants ─────────────────────────────────────────────────────

#[test]
fn detach_delete() {
    let prog = p("MATCH (n) DETACH DELETE n");
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            assert!(
                cq.left
                    .parts
                    .iter()
                    .any(|p| matches!(p, SimpleQueryStatement::Delete(_)))
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn nodetach_delete() {
    let prog = p("MATCH (n) NODETACH DELETE n");
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            assert!(
                cq.left
                    .parts
                    .iter()
                    .any(|p| matches!(p, SimpleQueryStatement::Delete(_)))
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }
}
