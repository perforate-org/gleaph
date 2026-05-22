//! §16.12 — Simplified path patterns.

use super::helpers::{first_edge_dir, graph_pat};
use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::EdgeDirection;

// ── Simplified path patterns ────────────────────────────────────────────

#[test]
fn basic_simplified() {
    let prog = p("MATCH (n)-/Friend/->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}

#[test]
fn simplified_union() {
    let prog = p("MATCH (n)-/Friend|Colleague/->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}

#[test]
fn simplified_conjunction() {
    let prog = p("MATCH (n)-/Friend&Active/->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}

#[test]
fn simplified_negation() {
    let prog = p("MATCH (n)-/!Blocked/->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}

#[test]
fn simplified_wildcard() {
    let prog = p("MATCH (n)-/%/->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}

#[test]
fn simplified_quantified() {
    let prog = p("MATCH (n)-/Friend{1,3}/->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}

#[test]
fn simplified_parenthesized() {
    let prog = p("MATCH (n)-/(Friend|Colleague)/->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}
