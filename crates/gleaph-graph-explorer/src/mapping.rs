//! The explicit boundary between Gleaph entity identities and `gpui-graph`
//! identities (§6 of the roadmap).
//!
//! Gleaph exposes opaque `element_id` bytes on the wire: 8-byte vertex ids and
//! 12-byte edge ids (see `gleaph_graph_kernel::federation::encoded`). The
//! explorer uses these bytes as the stable external keys for a
//! [`gpui_graph::KeyedGraph`], so the mapping is a direct byte → internal-id
//! correspondence that stays valid for the lifetime of the loaded scene.

use std::collections::HashMap;
use std::hash::BuildHasher;

use gpui_graph::{EdgeId, NodeId};

/// A stable Gleaph vertex identity as returned by `element_id(n)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct VertexIdentity(pub [u8; 8]);

/// A stable Gleaph edge identity as returned by `element_id(e)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EdgeIdentity(pub [u8; 12]);

impl VertexIdentity {
    /// Build from the raw bytes of an `element_id(n)` wire value.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }
}

impl EdgeIdentity {
    /// Build from the raw bytes of an `element_id(e)` wire value.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }
}

/// Maps Gleaph element identities to `gpui-graph` internal identities.
///
/// This is the single source of truth for correlating query results (which
/// carry Gleaph `element_id` bytes) with rendered graph elements. It is built
/// once when the graph is loaded and must not be reconstructed for overlay-only
/// changes.
///
/// The hasher `S` backs both identity maps; it defaults to SipHash
/// ([`std::collections::hash_map::RandomState`]).
#[derive(Debug, Default, Clone)]
pub struct GraphIdentityMap<S = std::collections::hash_map::RandomState> {
    vertex_to_node: HashMap<VertexIdentity, NodeId, S>,
    edge_to_edge: HashMap<EdgeIdentity, EdgeId, S>,
}

impl<S> GraphIdentityMap<S>
where
    S: BuildHasher + Default + Clone,
{
    /// Create an empty map.
    pub fn new() -> Self {
        Self::with_hasher(S::default())
    }

    /// Create an empty map with an explicit hasher.
    pub fn with_hasher(hasher: S) -> Self {
        Self {
            vertex_to_node: HashMap::with_hasher(hasher.clone()),
            edge_to_edge: HashMap::with_hasher(hasher),
        }
    }

    /// Record the correspondence between a Gleaph vertex identity and a
    /// rendered node.
    pub fn insert_vertex(&mut self, identity: VertexIdentity, node: NodeId) {
        self.vertex_to_node.insert(identity, node);
    }

    /// Record the correspondence between a Gleaph edge identity and a rendered
    /// edge.
    pub fn insert_edge(&mut self, identity: EdgeIdentity, edge: EdgeId) {
        self.edge_to_edge.insert(identity, edge);
    }

    /// Resolve a Gleaph vertex identity to a rendered node.
    pub fn node_for_vertex(&self, identity: &VertexIdentity) -> Option<NodeId> {
        self.vertex_to_node.get(identity).copied()
    }

    /// Resolve a Gleaph edge identity to a rendered edge.
    pub fn edge_for_edge(&self, identity: &EdgeIdentity) -> Option<EdgeId> {
        self.edge_to_edge.get(identity).copied()
    }

    /// Number of mapped vertices.
    pub fn vertex_count(&self) -> usize {
        self.vertex_to_node.len()
    }

    /// Number of mapped edges.
    pub fn edge_count(&self) -> usize {
        self.edge_to_edge.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_identity_from_bytes_accepts_exactly_8() {
        assert!(VertexIdentity::from_bytes(&[0; 8]).is_some());
        assert!(VertexIdentity::from_bytes(&[0; 7]).is_none());
        assert!(VertexIdentity::from_bytes(&[0; 9]).is_none());
    }

    #[test]
    fn edge_identity_from_bytes_accepts_exactly_12() {
        assert!(EdgeIdentity::from_bytes(&[0; 12]).is_some());
        assert!(EdgeIdentity::from_bytes(&[0; 11]).is_none());
    }

    #[test]
    fn map_roundtrips_vertex_and_edge() {
        let mut map = GraphIdentityMap::<std::collections::hash_map::RandomState>::new();
        let v = VertexIdentity([1; 8]);
        let e = EdgeIdentity([2; 12]);
        // Build a scene to obtain real NodeId/EdgeId values.
        let mut scene = gpui_graph::GraphScene::new();
        scene.merge(
            gpui_graph::GraphBatch::new()
                .node(v, "a")
                .node(VertexIdentity([3; 8]), "b")
                .edge(
                    e,
                    v,
                    VertexIdentity([3; 8]),
                    gpui_graph::EdgeDirection::Directed,
                    "x",
                ),
        );
        let node = scene.node_id(&v).unwrap();
        let edge = scene.edge_id(&e).unwrap();
        map.insert_vertex(v, node);
        map.insert_edge(e, edge);
        assert_eq!(map.node_for_vertex(&v), Some(node));
        assert_eq!(map.edge_for_edge(&e), Some(edge));
        assert_eq!(map.vertex_count(), 1);
        assert_eq!(map.edge_count(), 1);
    }

    #[test]
    fn with_hasher_builds_maps_with_chosen_hasher() {
        let mut map = GraphIdentityMap::with_hasher(rapidhash::fast::RandomState::default());
        let v = VertexIdentity([1; 8]);
        let e = EdgeIdentity([2; 12]);
        let mut scene = gpui_graph::GraphScene::new();
        scene.merge(
            gpui_graph::GraphBatch::new()
                .node(v, "a")
                .node(VertexIdentity([3; 8]), "b")
                .edge(
                    e,
                    v,
                    VertexIdentity([3; 8]),
                    gpui_graph::EdgeDirection::Directed,
                    "x",
                ),
        );
        let node = scene.node_id(&v).unwrap();
        let edge = scene.edge_id(&e).unwrap();
        map.insert_vertex(v, node);
        map.insert_edge(e, edge);
        assert_eq!(map.node_for_vertex(&v), Some(node));
        assert_eq!(map.edge_for_edge(&e), Some(edge));
    }
}
