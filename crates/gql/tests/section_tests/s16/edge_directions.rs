//! §16.7 — Path pattern edge directions.

use super::helpers::{first_edge_dir, graph_pat};
use crate::section_tests::p;
use gleaph_gql::types::EdgeDirection;

// ── Edge directions (full bracket patterns) ─────────────────────────────

#[test]
fn left_edge() {
    let dir = first_edge_dir("MATCH (n)<-[e]-(m) RETURN n");
    assert_eq!(dir, EdgeDirection::PointingLeft);
}

#[test]
fn any_direction_edge() {
    // -[e]- is "any direction"
    let prog = p("MATCH (n)-[e]-(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}

#[test]
fn left_or_right_edge() {
    let prog = p("MATCH (n)<-[e]->(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}

#[test]
fn undirected_edge() {
    let prog = p("MATCH (n)~[e]~(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}

#[test]
fn undirected_or_right_edge() {
    let prog = p("MATCH (n)~[e]~>(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}

#[test]
fn left_or_undirected_edge() {
    let prog = p("MATCH (n)<~[e]~(m) RETURN n");
    let gp = graph_pat(&prog);
    assert!(!gp.paths.is_empty());
}
