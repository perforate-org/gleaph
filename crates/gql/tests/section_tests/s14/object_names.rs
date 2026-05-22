//! §14 — Object names with slash-dot paths.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── Object name with slash-dot ──────────────────────────────────────────

#[test]
fn slash_dot_name() {
    let prog = p("USE /db/schema.graph MATCH (n) RETURN n");
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            assert!(matches!(
                &cq.left.parts[0],
                SimpleQueryStatement::Focused { .. }
            ));
        }
        other => panic!("expected Query with Focused, got {other:?}"),
    }
}
