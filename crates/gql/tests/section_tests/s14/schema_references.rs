//! §14.1 — Schema references in statements.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── Schema references ───────────────────────────────────────────────────

#[test]
fn relative_schema_ref() {
    let prog = p("AT ../other MATCH (n) RETURN n");
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            assert!(cq.left.at_schema.is_some());
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn absolute_schema_ref_multi_segment() {
    let prog = p("CREATE SCHEMA /db/schema1");
    let b = body(&prog);
    match &b.first {
        Statement::CreateSchema(cs) => {
            assert!(cs.name.parts.len() >= 2);
        }
        other => panic!("expected CreateSchema, got {other:?}"),
    }
}
