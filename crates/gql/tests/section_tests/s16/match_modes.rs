//! §16.4 — Graph pattern match modes.

use super::helpers::{first_edge_dir, graph_pat};
use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::EdgeDirection;

// ── Match modes ─────────────────────────────────────────────────────────

#[test]
fn repeatable_elements() {
    let prog = p("MATCH REPEATABLE ELEMENTS (n)-[e]->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(matches!(
        gp.match_mode,
        Some(MatchMode::RepeatableElements { .. })
    ));
}

#[test]
fn different_relationship() {
    let prog = p("MATCH DIFFERENT RELATIONSHIP (n)-[e]->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(matches!(
        gp.match_mode,
        Some(MatchMode::DifferentEdges { .. })
    ));
}

#[test]
fn different_edges() {
    let prog = p("MATCH DIFFERENT EDGES (n)-[e]->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(matches!(
        gp.match_mode,
        Some(MatchMode::DifferentEdges { .. })
    ));
}

#[test]
fn different_relationships() {
    let prog = p("MATCH DIFFERENT RELATIONSHIPS (n)-[e]->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(matches!(
        gp.match_mode,
        Some(MatchMode::DifferentEdges { .. })
    ));
}

#[test]
fn different_edge_bindings() {
    let prog = p("MATCH DIFFERENT EDGE BINDINGS (n)-[e]->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(matches!(
        gp.match_mode,
        Some(MatchMode::DifferentEdges { .. })
    ));
}

#[test]
fn different_relationship_bindings() {
    let prog = p("MATCH DIFFERENT RELATIONSHIP BINDINGS (n)-[e]->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(matches!(
        gp.match_mode,
        Some(MatchMode::DifferentEdges { .. })
    ));
}
