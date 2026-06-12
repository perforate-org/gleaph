//! §16.5 — INSERT graph pattern edges.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::EdgeDirection;

// ── Insert edge with IS keyword ─────────────────────────────────────────

#[test]
fn insert_edge_with_is() {
    let prog = p("INSERT (a)-[e IS Friend]->(b)");
    let b = body(&prog);
    match &b.first {
        Statement::Insert(ins) => {
            assert!(!ins.patterns.is_empty());
            // Should have node-edge-node (3 elements)
            assert_eq!(ins.patterns[0].elements.len(), 3);
            if let InsertElement::Edge(e) = &ins.patterns[0].elements[1] {
                assert_eq!(e.is_or_colon, Some(IsOrColon::Is));
            } else {
                panic!("expected Edge");
            }
        }
        other => panic!("expected Insert, got {other:?}"),
    }
}

#[test]
fn insert_edge_left() {
    let prog = p("INSERT (a)<-[e :Friend]-(b)");
    let b = body(&prog);
    match &b.first {
        Statement::Insert(ins) => {
            if let InsertElement::Edge(e) = &ins.patterns[0].elements[1] {
                assert_eq!(e.direction, EdgeDirection::PointingLeft);
            } else {
                panic!("expected Edge");
            }
        }
        other => panic!("expected Insert, got {other:?}"),
    }
}

#[test]
fn insert_edge_undirected() {
    let prog = p("INSERT (a)~[e :Friend]~(b)");
    let b = body(&prog);
    match &b.first {
        Statement::Insert(ins) => {
            if let InsertElement::Edge(e) = &ins.patterns[0].elements[1] {
                assert_eq!(e.direction, EdgeDirection::Undirected);
            } else {
                panic!("expected Edge");
            }
        }
        other => panic!("expected Insert, got {other:?}"),
    }
}
