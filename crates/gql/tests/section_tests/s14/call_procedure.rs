//! §14.5 — CALL procedure statement.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── CALL procedure ──────────────────────────────────────────────────────

#[test]
fn call_no_args() {
    let prog = p("CALL myProc");
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            assert!(matches!(
                &cq.left.parts[0],
                SimpleQueryStatement::CallProcedure(_)
            ));
        }
        other => panic!("expected Query with CallProcedure, got {other:?}"),
    }
}
