//! Logical property graph model.
//!
//! This module owns the logical topology and data of a graph. It deliberately
//! contains no visual information: node positions, styling, and layout state
//! belong to the scene and layout layers (see `DESIGN.md` §6, Invariant 1).
//!
//! Nodes and edges use stable generational identifiers (`slotmap`). Deletion
//! followed by slot reuse must never cause a stale reference to silently point
//! at a different entity (Invariant 3).

use std::sync::Arc;

use slotmap::{DenseSlotMap, new_key_type};

new_key_type! {
    /// Stable identity of a node for the lifetime of the entity.
    pub struct NodeId;
    /// Stable identity of an edge for the lifetime of the entity.
    pub struct EdgeId;
}

/// Whether an edge is directed or undirected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeDirection {
    /// A directed edge from `source` to `target`.
    Directed,
    /// An undirected edge between `source` and `target`.
    Undirected,
}

/// A node in the logical graph.
///
/// Visual information such as position does not belong here (Invariant 1).
#[derive(Debug, Clone)]
pub struct Node<N> {
    /// Application data attached to the node.
    pub data: N,
    incident_edges: Vec<EdgeId>,
}

/// An edge in the logical graph.
///
/// Edges have independent identity. The `(source, target)` tuple must never be
/// treated as edge identity, which is required for parallel edges and property
/// graph semantics (§6.3).
#[derive(Debug, Clone)]
pub struct Edge<E> {
    /// Source node.
    pub source: NodeId,
    /// Target node.
    pub target: NodeId,
    /// Directionality of the edge.
    pub direction: EdgeDirection,
    /// Application data attached to the edge.
    pub data: E,
}

/// A description of the changes applied to a graph, expressed using internal
/// identities (§8.3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphDelta {
    /// Nodes added by the mutation.
    pub added_nodes: Vec<NodeId>,
    /// Nodes whose data was updated.
    pub updated_nodes: Vec<NodeId>,
    /// Nodes removed by the mutation.
    pub removed_nodes: Vec<NodeId>,
    /// Edges added by the mutation.
    pub added_edges: Vec<EdgeId>,
    /// Edges whose data was updated.
    pub updated_edges: Vec<EdgeId>,
    /// Edges removed by the mutation.
    pub removed_edges: Vec<EdgeId>,
    /// Whether the mutation changed graph topology.
    ///
    /// An updated edge can change its endpoints or direction while retaining
    /// its stable identity. In that case the edge appears in
    /// `updated_edges`, and this marker tells consumers that topology-derived
    /// state must be rebuilt.
    pub topology_changed: bool,
}

impl GraphDelta {
    /// Whether the delta contains no changes.
    pub fn is_empty(&self) -> bool {
        self.added_nodes.is_empty()
            && self.updated_nodes.is_empty()
            && self.removed_nodes.is_empty()
            && self.added_edges.is_empty()
            && self.updated_edges.is_empty()
            && self.removed_edges.is_empty()
            && !self.topology_changed
    }

    /// Merge another delta into this one, appending its entries.
    pub fn extend(&mut self, other: GraphDelta) {
        self.added_nodes.extend(other.added_nodes);
        self.updated_nodes.extend(other.updated_nodes);
        self.removed_nodes.extend(other.removed_nodes);
        self.added_edges.extend(other.added_edges);
        self.updated_edges.extend(other.updated_edges);
        self.removed_edges.extend(other.removed_edges);
        self.topology_changed |= other.topology_changed;
    }
}

/// A logical property graph with stable generational identity.
///
/// The graph is normally a client-side working subgraph rather than the
/// complete database graph (§6.5).
#[derive(Debug)]
pub struct Graph<N = (), E = ()> {
    nodes: DenseSlotMap<NodeId, Node<N>>,
    edges: DenseSlotMap<EdgeId, Edge<E>>,
    /// Identity of this logical graph source. It is deliberately not shared
    /// by `Clone`: a cloned graph may diverge independently from its source.
    source_identity: Arc<()>,
}

impl<N: Clone, E: Clone> Clone for Graph<N, E> {
    fn clone(&self) -> Self {
        Self {
            nodes: self.nodes.clone(),
            edges: self.edges.clone(),
            source_identity: Arc::new(()),
        }
    }
}

impl<N, E> Default for Graph<N, E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<N, E> Graph<N, E> {
    /// Create an empty graph.
    pub fn new() -> Self {
        Self {
            nodes: DenseSlotMap::with_key(),
            edges: DenseSlotMap::with_key(),
            source_identity: Arc::new(()),
        }
    }

    /// Return the private identity token used by derived graph state.
    pub(crate) fn source_identity(&self) -> Arc<()> {
        Arc::clone(&self.source_identity)
    }

    /// Number of nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges in the graph.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Whether the graph contains no nodes and no edges.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }

    /// Add a node with the given data, returning its stable identity.
    pub fn add_node(&mut self, data: N) -> NodeId {
        self.nodes.insert(Node {
            data,
            incident_edges: Vec::new(),
        })
    }

    /// Add an edge between `source` and `target`, returning its stable identity.
    ///
    /// Both endpoints must exist in the graph; otherwise `None` is returned and
    /// no mutation occurs.
    pub fn add_edge(
        &mut self,
        source: NodeId,
        target: NodeId,
        direction: EdgeDirection,
        data: E,
    ) -> Option<EdgeId> {
        if !self.nodes.contains_key(source) || !self.nodes.contains_key(target) {
            return None;
        }
        let id = self.edges.insert(Edge {
            source,
            target,
            direction,
            data,
        });
        self.nodes.get_mut(source).unwrap().incident_edges.push(id);
        if target != source {
            self.nodes.get_mut(target).unwrap().incident_edges.push(id);
        }
        Some(id)
    }

    /// Look up a node by identity.
    pub fn node(&self, id: NodeId) -> Option<&Node<N>> {
        self.nodes.get(id)
    }

    /// Look up a node's data by identity.
    pub fn node_data(&self, id: NodeId) -> Option<&N> {
        self.nodes.get(id).map(|n| &n.data)
    }

    /// Look up an edge by identity.
    pub fn edge(&self, id: EdgeId) -> Option<&Edge<E>> {
        self.edges.get(id)
    }

    /// Look up an edge's data by identity.
    pub fn edge_data(&self, id: EdgeId) -> Option<&E> {
        self.edges.get(id).map(|e| &e.data)
    }

    /// The incident edge identifiers of a node, in insertion order.
    pub fn incident_edges(&self, id: NodeId) -> Option<&[EdgeId]> {
        self.nodes.get(id).map(|n| n.incident_edges.as_slice())
    }

    /// Iterate over all nodes and their identities.
    pub fn nodes(&self) -> impl Iterator<Item = (NodeId, &Node<N>)> {
        self.nodes.iter()
    }

    /// Iterate over all edges and their identities.
    pub fn edges(&self) -> impl Iterator<Item = (EdgeId, &Edge<E>)> {
        self.edges.iter()
    }

    /// Replace a node's data, returning a delta describing the update.
    ///
    /// Returns `None` if the node does not exist.
    pub fn update_node_data(&mut self, id: NodeId, data: N) -> Option<GraphDelta> {
        let node = self.nodes.get_mut(id)?;
        node.data = data;
        Some(GraphDelta {
            updated_nodes: vec![id],
            ..GraphDelta::default()
        })
    }

    /// Replace an edge's data, returning a delta describing the update.
    ///
    /// Returns `None` if the edge does not exist.
    pub fn update_edge_data(&mut self, id: EdgeId, data: E) -> Option<GraphDelta> {
        let edge = self.edges.get_mut(id)?;
        edge.data = data;
        Some(GraphDelta {
            updated_edges: vec![id],
            ..GraphDelta::default()
        })
    }

    /// Replace an edge's endpoints, direction, and data while preserving its
    /// stable identity.
    ///
    /// Both new endpoints are validated before any graph state is mutated. If
    /// an endpoint or the edge is unknown, `None` is returned and the graph is
    /// unchanged. Endpoint changes update each affected node's incident list;
    /// a self-loop is recorded exactly once. Direction changes do not alter
    /// adjacency, but still count as topology changes for layout consumers.
    pub fn update_edge(
        &mut self,
        id: EdgeId,
        source: NodeId,
        target: NodeId,
        direction: EdgeDirection,
        data: E,
    ) -> Option<GraphDelta> {
        if !self.nodes.contains_key(source) || !self.nodes.contains_key(target) {
            return None;
        }

        let edge = self.edges.get(id)?;
        let old_source = edge.source;
        let old_target = edge.target;
        let topology_changed =
            old_source != source || old_target != target || edge.direction != direction;

        if old_source != source || old_target != target {
            for endpoint in [old_source, old_target] {
                if let Some(node) = self.nodes.get_mut(endpoint) {
                    node.incident_edges.retain(|edge_id| *edge_id != id);
                }
            }
            self.nodes.get_mut(source).unwrap().incident_edges.push(id);
            if target != source {
                self.nodes.get_mut(target).unwrap().incident_edges.push(id);
            }
        }

        let edge = self.edges.get_mut(id).unwrap();
        edge.source = source;
        edge.target = target;
        edge.direction = direction;
        edge.data = data;

        Some(GraphDelta {
            updated_edges: vec![id],
            topology_changed,
            ..GraphDelta::default()
        })
    }

    /// Remove a node and all of its incident edges.
    ///
    /// Returns a delta describing the removal, or `None` if the node does not
    /// exist. Removal performs an `O(degree)` pass over each affected node's
    /// incident edge list (§6.5).
    pub fn remove_node(&mut self, id: NodeId) -> Option<GraphDelta> {
        let node = self.nodes.remove(id)?;
        let mut delta = GraphDelta {
            removed_nodes: vec![id],
            topology_changed: true,
            ..GraphDelta::default()
        };

        for edge_id in &node.incident_edges {
            if let Some(edge) = self.edges.remove(*edge_id) {
                delta.removed_edges.push(*edge_id);
                // Remove the edge from the other endpoint's incident list.
                let other = if edge.source == id {
                    edge.target
                } else {
                    edge.source
                };
                if let Some(other_node) = self.nodes.get_mut(other) {
                    other_node.incident_edges.retain(|e| *e != *edge_id);
                }
            }
        }

        Some(delta)
    }

    /// Remove an edge.
    ///
    /// Returns a delta describing the removal, or `None` if the edge does not
    /// exist.
    pub fn remove_edge(&mut self, id: EdgeId) -> Option<GraphDelta> {
        let edge = self.edges.remove(id)?;
        for endpoint in [edge.source, edge.target] {
            if let Some(node) = self.nodes.get_mut(endpoint) {
                node.incident_edges.retain(|e| *e != id);
            }
        }
        Some(GraphDelta {
            removed_edges: vec![id],
            topology_changed: true,
            ..GraphDelta::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_lookup_nodes_and_edges() {
        let mut g = Graph::new();
        let a = g.add_node("a");
        let b = g.add_node("b");
        let e = g.add_edge(a, b, EdgeDirection::Directed, "e").unwrap();

        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
        assert_eq!(g.node_data(a), Some(&"a"));
        assert_eq!(g.edge_data(e), Some(&"e"));
        assert_eq!(g.edge(e).unwrap().source, a);
        assert_eq!(g.edge(e).unwrap().target, b);
        assert_eq!(g.incident_edges(a).unwrap(), &[e]);
        assert_eq!(g.incident_edges(b).unwrap(), &[e]);
    }

    #[test]
    fn add_edge_requires_existing_endpoints() {
        let mut g = Graph::new();
        let a = g.add_node("a");
        let ghost = NodeId::default();
        assert!(g.add_edge(a, ghost, EdgeDirection::Directed, ()).is_none());
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn self_loop_is_incident_once() {
        let mut g = Graph::new();
        let a = g.add_node("a");
        let e = g.add_edge(a, a, EdgeDirection::Directed, ()).unwrap();
        assert_eq!(g.incident_edges(a).unwrap(), &[e]);
    }

    #[test]
    fn parallel_edges_have_distinct_identity() {
        let mut g = Graph::new();
        let a = g.add_node("a");
        let b = g.add_node("b");
        let e1 = g.add_edge(a, b, EdgeDirection::Directed, 1).unwrap();
        let e2 = g.add_edge(a, b, EdgeDirection::Directed, 2).unwrap();
        assert_ne!(e1, e2);
        assert_eq!(g.edge_data(e1), Some(&1));
        assert_eq!(g.edge_data(e2), Some(&2));
        assert_eq!(g.incident_edges(a).unwrap(), &[e1, e2]);
    }

    #[test]
    fn remove_node_removes_incident_edges() {
        let mut g = Graph::new();
        let a = g.add_node("a");
        let b = g.add_node("b");
        let c = g.add_node("c");
        let ab = g.add_edge(a, b, EdgeDirection::Directed, ()).unwrap();
        let ac = g.add_edge(a, c, EdgeDirection::Directed, ()).unwrap();

        let delta = g.remove_node(a).unwrap();
        assert_eq!(delta.removed_nodes, vec![a]);
        assert_eq!(delta.removed_edges.len(), 2);
        assert!(delta.removed_edges.contains(&ab));
        assert!(delta.removed_edges.contains(&ac));
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 0);
        assert_eq!(g.incident_edges(b).unwrap(), &[]);
        assert_eq!(g.incident_edges(c).unwrap(), &[]);
    }

    #[test]
    fn remove_edge_updates_both_endpoints() {
        let mut g = Graph::new();
        let a = g.add_node("a");
        let b = g.add_node("b");
        let e = g.add_edge(a, b, EdgeDirection::Undirected, ()).unwrap();

        let delta = g.remove_edge(e).unwrap();
        assert_eq!(delta.removed_edges, vec![e]);
        assert_eq!(g.edge_count(), 0);
        assert_eq!(g.incident_edges(a).unwrap(), &[]);
        assert_eq!(g.incident_edges(b).unwrap(), &[]);
    }

    #[test]
    fn stale_id_does_not_reference_reused_slot() {
        let mut g: Graph<&str, ()> = Graph::new();
        let a = g.add_node("a");
        let b = g.add_node("b");
        g.remove_node(a).unwrap();
        // Reuse the freed slot; the stale `a` id must not resolve to the new node.
        let c = g.add_node("c");
        assert_ne!(a, c);
        assert!(g.node(a).is_none());
        assert_eq!(g.node_data(c), Some(&"c"));
        assert_eq!(g.node_data(b), Some(&"b"));
    }

    #[test]
    fn update_data_produces_delta() {
        let mut g = Graph::new();
        let a = g.add_node(1);
        let e = g.add_edge(a, a, EdgeDirection::Directed, 10).unwrap();

        let nd = g.update_node_data(a, 2).unwrap();
        assert_eq!(nd.updated_nodes, vec![a]);
        let ed = g.update_edge_data(e, 20).unwrap();
        assert_eq!(ed.updated_edges, vec![e]);
        assert_eq!(g.node_data(a), Some(&2));
        assert_eq!(g.edge_data(e), Some(&20));
    }

    #[test]
    fn update_edge_replaces_topology_without_losing_identity_or_adjacency() {
        let mut g = Graph::new();
        let a = g.add_node("a");
        let b = g.add_node("b");
        let c = g.add_node("c");
        let edge = g.add_edge(a, a, EdgeDirection::Directed, "old").unwrap();

        let delta = g
            .update_edge(edge, b, c, EdgeDirection::Undirected, "new")
            .unwrap();
        assert_eq!(delta.updated_edges, vec![edge]);
        assert!(delta.topology_changed);
        assert_eq!(g.edge(edge).unwrap().source, b);
        assert_eq!(g.edge(edge).unwrap().target, c);
        assert_eq!(g.edge(edge).unwrap().direction, EdgeDirection::Undirected);
        assert_eq!(g.edge_data(edge), Some(&"new"));
        assert_eq!(g.incident_edges(a).unwrap(), &[]);
        assert_eq!(g.incident_edges(b).unwrap(), &[edge]);
        assert_eq!(g.incident_edges(c).unwrap(), &[edge]);

        let delta = g
            .update_edge(edge, c, c, EdgeDirection::Directed, "newer")
            .unwrap();
        assert!(delta.topology_changed);
        assert_eq!(g.edge(edge).unwrap().source, c);
        assert_eq!(g.edge(edge).unwrap().target, c);
        assert_eq!(g.edge(edge).unwrap().direction, EdgeDirection::Directed);
        assert_eq!(g.incident_edges(b).unwrap(), &[]);
        assert_eq!(g.incident_edges(c).unwrap(), &[edge]);
    }

    #[test]
    fn update_edge_preserves_parallel_sibling_adjacency() {
        let mut g = Graph::new();
        let a = g.add_node("a");
        let b = g.add_node("b");
        let c = g.add_node("c");
        let updated = g
            .add_edge(a, b, EdgeDirection::Directed, "updated")
            .unwrap();
        let sibling = g
            .add_edge(a, b, EdgeDirection::Undirected, "sibling")
            .unwrap();

        let delta = g
            .update_edge(updated, a, c, EdgeDirection::Directed, "updated-again")
            .unwrap();

        assert_eq!(delta.updated_edges, vec![updated]);
        assert!(delta.topology_changed);
        assert_eq!(g.edge_count(), 2);
        assert_eq!(g.edge(updated).unwrap().target, c);
        assert_eq!(g.edge(sibling).unwrap().source, a);
        assert_eq!(g.edge(sibling).unwrap().target, b);
        assert_eq!(g.incident_edges(a).unwrap(), &[sibling, updated]);
        assert_eq!(g.incident_edges(b).unwrap(), &[sibling]);
        assert_eq!(g.incident_edges(c).unwrap(), &[updated]);
    }

    #[test]
    fn update_edge_rejects_unknown_endpoint_atomically() {
        let mut g = Graph::new();
        let a = g.add_node("a");
        let b = g.add_node("b");
        let edge = g.add_edge(a, b, EdgeDirection::Directed, "old").unwrap();
        let unknown = NodeId::default();

        assert!(
            g.update_edge(edge, a, unknown, EdgeDirection::Undirected, "new")
                .is_none()
        );
        assert_eq!(g.edge(edge).unwrap().source, a);
        assert_eq!(g.edge(edge).unwrap().target, b);
        assert_eq!(g.edge(edge).unwrap().direction, EdgeDirection::Directed);
        assert_eq!(g.edge_data(edge), Some(&"old"));
        assert_eq!(g.incident_edges(a).unwrap(), &[edge]);
        assert_eq!(g.incident_edges(b).unwrap(), &[edge]);
    }

    #[test]
    fn update_edge_data_only_does_not_mark_topology() {
        let mut g = Graph::new();
        let a = g.add_node("a");
        let b = g.add_node("b");
        let edge = g.add_edge(a, b, EdgeDirection::Directed, "old").unwrap();

        let delta = g
            .update_edge(edge, a, b, EdgeDirection::Directed, "new")
            .unwrap();
        assert_eq!(delta.updated_edges, vec![edge]);
        assert!(!delta.topology_changed);
        assert_eq!(g.incident_edges(a).unwrap(), &[edge]);
        assert_eq!(g.incident_edges(b).unwrap(), &[edge]);
    }

    #[test]
    fn delta_extend_merges() {
        let mut d = GraphDelta::default();
        d.extend(GraphDelta {
            added_nodes: vec![NodeId::default()],
            ..GraphDelta::default()
        });
        d.extend(GraphDelta {
            removed_edges: vec![EdgeId::default()],
            topology_changed: true,
            ..GraphDelta::default()
        });
        assert_eq!(d.added_nodes.len(), 1);
        assert_eq!(d.removed_edges.len(), 1);
        assert!(d.topology_changed);
        assert!(!d.is_empty());
    }

    #[test]
    fn delta_extend_preserves_topology_after_data_only_delta() {
        let mut d = GraphDelta::default();
        d.extend(GraphDelta {
            topology_changed: true,
            ..GraphDelta::default()
        });
        d.extend(GraphDelta {
            updated_nodes: vec![NodeId::default()],
            ..GraphDelta::default()
        });

        assert!(d.topology_changed);
        assert_eq!(d.updated_nodes.len(), 1);
    }
}
