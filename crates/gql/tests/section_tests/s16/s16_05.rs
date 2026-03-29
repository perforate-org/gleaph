//! §16.5 — Insert graph pattern.
//!
//! GQL rules: insertGraphPattern, insertPathPattern, insertElement.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;
use gleaph_gql::types::EdgeDirection;

/// Helper to extract the InsertStatement from a top-level INSERT statement.
fn ins(input: &str) -> InsertStatement {
    let prog = p(input);
    let b = body(&prog);
    match &b.first {
        Statement::Insert(i) => i.clone(),
        Statement::Query(cq) => {
            for part in &cq.left.parts {
                if let SimpleQueryStatement::Insert(i) = part {
                    return i.clone();
                }
            }
            panic!("no Insert found in query parts: {:?}", cq.left.parts);
        }
        other => panic!("expected Insert or Query, got {other:?}"),
    }
}

// ── insertGraphPattern ──────────────────────────────────────────────────
mod insert_graph_pattern {
    use super::*;

    /// INSERT (a)-[:KNOWS]->(b) — directed right edge
    #[test]
    fn insert_directed_right() {
        let i = ins("INSERT (a)-[:KNOWS]->(b)");
        assert!(
            !i.patterns.is_empty(),
            "expected at least one insert pattern"
        );
        let pat = &i.patterns[0];
        let edge = pat
            .elements
            .iter()
            .find_map(|e| match e {
                InsertElement::Edge(ep) => Some(ep),
                _ => None,
            })
            .expect("expected an edge element");
        assert_eq!(
            edge.direction,
            EdgeDirection::PointingRight,
            "expected PointingRight"
        );
        assert_eq!(edge.labels, vec!["KNOWS".to_string()]);
    }

    /// INSERT (a)~[:KNOWS]~(b) — undirected edge
    #[test]
    fn insert_undirected() {
        let i = ins("INSERT (a)~[:KNOWS]~(b)");
        let pat = &i.patterns[0];
        let edge = pat
            .elements
            .iter()
            .find_map(|e| match e {
                InsertElement::Edge(ep) => Some(ep),
                _ => None,
            })
            .expect("expected an edge element");
        assert_eq!(
            edge.direction,
            EdgeDirection::Undirected,
            "expected Undirected"
        );
    }

    /// INSERT (a)<-[:KNOWS]-(b) — directed left edge
    #[test]
    fn insert_directed_left() {
        let i = ins("INSERT (a)<-[:KNOWS]-(b)");
        let pat = &i.patterns[0];
        let edge = pat
            .elements
            .iter()
            .find_map(|e| match e {
                InsertElement::Edge(ep) => Some(ep),
                _ => None,
            })
            .expect("expected an edge element");
        assert_eq!(
            edge.direction,
            EdgeDirection::PointingLeft,
            "expected PointingLeft"
        );
    }

    /// INSERT (a IS Person) — node with IS keyword
    #[test]
    fn insert_node_with_is() {
        let i = ins("INSERT (a IS Person)");
        let node = i.patterns[0]
            .elements
            .iter()
            .find_map(|e| match e {
                InsertElement::Node(np) => Some(np),
                _ => None,
            })
            .expect("expected a node element");
        assert_eq!(node.is_or_colon, Some(IsOrColon::Is));
        assert_eq!(node.labels, vec!["Person".to_string()]);
    }

    /// INSERT (a:Person {name: 'Alice'}) — node with properties
    #[test]
    fn insert_node_with_properties() {
        let i = ins("INSERT (a:Person {name: 'Alice'})");
        let node = i.patterns[0]
            .elements
            .iter()
            .find_map(|e| match e {
                InsertElement::Node(np) => Some(np),
                _ => None,
            })
            .expect("expected a node element");
        assert!(!node.properties.is_empty());
        assert_eq!(node.properties[0].name, "name");
    }

    /// INSERT (a)-[IS KNOWS]->(b) — edge with IS keyword
    #[test]
    fn insert_edge_with_is() {
        let i = ins("INSERT (a)-[IS KNOWS]->(b)");
        let edge = i.patterns[0]
            .elements
            .iter()
            .find_map(|e| match e {
                InsertElement::Edge(ep) => Some(ep),
                _ => None,
            })
            .expect("expected an edge element");
        assert_eq!(edge.is_or_colon, Some(IsOrColon::Is));
        assert_eq!(edge.labels, vec!["KNOWS".to_string()]);
    }

    /// INSERT (a)-[:KNOWS {since: 2020}]->(b) — edge with properties
    #[test]
    fn insert_edge_with_properties() {
        let i = ins("INSERT (a)-[:KNOWS {since: 2020}]->(b)");
        let edge = i.patterns[0]
            .elements
            .iter()
            .find_map(|e| match e {
                InsertElement::Edge(ep) => Some(ep),
                _ => None,
            })
            .expect("expected an edge element");
        assert!(!edge.properties.is_empty());
        assert_eq!(edge.properties[0].name, "since");
    }

    /// INSERT (a:Person&Employee) — node with multiple labels
    #[test]
    fn insert_node_multiple_labels() {
        let i = ins("INSERT (a:Person&Employee)");
        let node = i.patterns[0]
            .elements
            .iter()
            .find_map(|e| match e {
                InsertElement::Node(np) => Some(np),
                _ => None,
            })
            .expect("expected a node element");
        assert_eq!(
            node.labels,
            vec!["Person".to_string(), "Employee".to_string()]
        );
    }

    /// INSERT (a)-[:KNOWS]->(b)-[:LIKES]->(c) — multi-hop insert
    #[test]
    fn insert_multi_hop() {
        let i = ins("INSERT (a)-[:KNOWS]->(b)-[:LIKES]->(c)");
        let pat = &i.patterns[0];
        // Should have 5 elements: node, edge, node, edge, node
        assert_eq!(pat.elements.len(), 5);
    }
}
