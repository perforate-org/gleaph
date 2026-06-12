//! §16.8 — Label expressions.

use super::helpers::graph_pat;
use crate::section_tests::p;

// ── Parenthesized label expression ──────────────────────────────────────

#[test]
fn parenthesized_label() {
    let prog = p("MATCH (n :(Person|Employee)) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}
