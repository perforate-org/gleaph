//! Query-result presentation (§8 of the roadmap).
//!
//! Converts a successful query result into a visual presentation model and
//! maps it onto [`gpui_graph::OverlayCategory`]. The explorer never makes
//! `gpui-graph` understand query semantics; it only resolves which rendered
//! elements are emphasized versus dimmed.
//!
//! The initial semantics are `context` and `result` (the roadmap's collapsed
//! model): elements returned by the query are `Emphasized`, and everything else
//! is `Dimmed`. `matched` and property semantics are added in a later slice.

use std::collections::HashSet;

use gpui_graph::{EdgeId, NodeId, OverlayCategory};

use crate::mapping::{EdgeIdentity, GraphIdentityMap, VertexIdentity};

/// A resolved presentation: which rendered nodes and edges to emphasize.
///
/// The presentation is derived from a query result plus the identity map. It is
/// applied to the view's overlay resolvers without rebuilding the scene.
#[derive(Debug, Default)]
pub struct Presentation {
    /// Rendered nodes to emphasize.
    pub emphasized_nodes: HashSet<NodeId>,
    /// Rendered edges to emphasize.
    pub emphasized_edges: HashSet<EdgeId>,
}

impl Presentation {
    /// Whether the presentation emphasizes nothing.
    pub fn is_empty(&self) -> bool {
        self.emphasized_nodes.is_empty() && self.emphasized_edges.is_empty()
    }

    /// Resolve the overlay category for a node.
    pub fn node_category(&self, node: NodeId) -> OverlayCategory {
        if self.emphasized_nodes.contains(&node) {
            OverlayCategory::Emphasized
        } else {
            OverlayCategory::Dimmed
        }
    }

    /// Resolve the overlay category for an edge.
    pub fn edge_category(&self, edge: EdgeId) -> OverlayCategory {
        if self.emphasized_edges.contains(&edge) {
            OverlayCategory::Emphasized
        } else {
            OverlayCategory::Dimmed
        }
    }
}

/// Build a presentation from the element identities a query returned.
///
/// Each returned vertex/edge identity is resolved through the identity map to a
/// rendered element and marked emphasized. When the result is empty, the
/// presentation is empty (the caller may choose to keep the base style rather
/// than dim everything).
pub fn build_presentation(
    map: &GraphIdentityMap,
    vertices: &[VertexIdentity],
    edges: &[EdgeIdentity],
) -> Presentation {
    let mut presentation = Presentation::default();
    for identity in vertices {
        if let Some(node) = map.node_for_vertex(identity) {
            presentation.emphasized_nodes.insert(node);
        }
    }
    for identity in edges {
        if let Some(edge) = map.edge_for_edge(identity) {
            presentation.emphasized_edges.insert(edge);
        }
    }
    presentation
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{EdgeRecord, ExplorerEdge, ExplorerNode, VertexRecord, build_graph};
    use crate::mapping::{EdgeIdentity, VertexIdentity};

    fn vid(n: u8) -> VertexIdentity {
        VertexIdentity([n; 8])
    }

    fn eid(n: u8) -> EdgeIdentity {
        EdgeIdentity([n; 12])
    }

    fn scene_with_map() -> (
        gpui_graph::GraphScene<VertexIdentity, EdgeIdentity, ExplorerNode, ExplorerEdge>,
        GraphIdentityMap,
    ) {
        let load = build_graph(
            vec![
                VertexRecord {
                    identity: vid(1),
                    label: "User".into(),
                    display: "alice".into(),
                },
                VertexRecord {
                    identity: vid(2),
                    label: "User".into(),
                    display: "bob".into(),
                },
            ],
            vec![EdgeRecord {
                identity: eid(1),
                source: vid(1),
                target: vid(2),
                label: "FOLLOWS".into(),
            }],
        );
        let mut scene = gpui_graph::GraphScene::new();
        scene.merge(load.batch.clone());
        let map = crate::graph::build_map(&scene, &load);
        (scene, map)
    }

    #[test]
    fn presentation_emphasizes_returned_elements_and_dims_rest() {
        let (_scene, map) = scene_with_map();
        let node1 = map.node_for_vertex(&vid(1)).unwrap();
        let node2 = map.node_for_vertex(&vid(2)).unwrap();
        let edge = map.edge_for_edge(&eid(1)).unwrap();

        let presentation = build_presentation(&map, &[vid(1)], &[eid(1)]);
        assert_eq!(
            presentation.node_category(node1),
            OverlayCategory::Emphasized
        );
        assert_eq!(presentation.node_category(node2), OverlayCategory::Dimmed);
        assert_eq!(
            presentation.edge_category(edge),
            OverlayCategory::Emphasized
        );
    }

    #[test]
    fn presentation_empty_when_no_identities() {
        let (_scene, map) = scene_with_map();
        let presentation = build_presentation(&map, &[], &[]);
        assert!(presentation.is_empty());
    }

    #[test]
    fn presentation_ignores_unmapped_identities() {
        let (_scene, map) = scene_with_map();
        let presentation = build_presentation(&map, &[vid(99)], &[eid(99)]);
        assert!(presentation.is_empty());
    }
}
