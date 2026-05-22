//! §16.8 — Label expressions.

use super::helpers::{first_edge_dir, graph_pat};
use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::EdgeDirection;

// ── Parenthesized label expression ──────────────────────────────────────

#[test]
fn parenthesized_label() {
    let prog = p("MATCH (n :(Person|Employee)) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}
