//! §14 — SET / REMOVE label variants.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── SET/REMOVE label variants ───────────────────────────────────────────

#[test]
fn set_label_with_is() {
    let prog = p("MATCH (n) SET n IS Person");
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            assert!(
                cq.left
                    .parts
                    .iter()
                    .any(|p| matches!(p, SimpleQueryStatement::Set(_)))
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn set_label_with_colon() {
    let prog = p("MATCH (n) SET n :Person");
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            assert!(
                cq.left
                    .parts
                    .iter()
                    .any(|p| matches!(p, SimpleQueryStatement::Set(_)))
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn remove_label_with_is() {
    let prog = p("MATCH (n) REMOVE n IS Person");
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            assert!(
                cq.left
                    .parts
                    .iter()
                    .any(|p| matches!(p, SimpleQueryStatement::Remove(_)))
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn remove_label_with_colon() {
    let prog = p("MATCH (n) REMOVE n :Person");
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            assert!(
                cq.left
                    .parts
                    .iter()
                    .any(|p| matches!(p, SimpleQueryStatement::Remove(_)))
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }
}
