//! Keyed graph: maps external application identity to internal graph identity.
//!
//! `gpui-graph` internal identity must remain independent from application or
//! database identity (§7). A [`KeyedGraph`] owns that translation: it keeps a
//! [`Graph`] plus maps from external node/edge keys to internal
//! [`NodeId`]/[`EdgeId`], and applies [`GraphBatch`]es and [`GraphPatch`]es
//! expressed in external keys, producing a [`GraphDelta`] in internal identity.
//!
//! The graph and both key maps are one consistency boundary. Topology changes
//! must flow through [`KeyedGraph::merge`] or [`KeyedGraph::apply`], which keep
//! the maps synchronized with graph removals and stable edge identities. The
//! underlying graph is intentionally exposed only through a shared reference;
//! a raw mutable escape would allow callers to bypass the key maps.

use std::hash::BuildHasher;

use crate::graph::{EdgeDirection, EdgeId, Graph, GraphDelta, NodeId};
use crate::hash::HashMap;
use crate::patch::{EdgePatch, GraphBatch, GraphPatch, NodePatch};

/// A graph with external key mapping (§7).
///
/// `KeyedGraph` owns the graph-to-key correspondence. Use [`Self::merge`] or
/// [`Self::apply`] for all mutations so node and edge key maps cannot diverge
/// from graph topology.
///
/// The hasher `S` backs both external-key maps; it defaults to SipHash
/// ([`std::collections::hash_map::RandomState`]).
#[derive(Debug, Clone)]
pub struct KeyedGraph<NK, EK, N = (), E = (), S = std::collections::hash_map::RandomState> {
    graph: Graph<N, E>,
    node_keys: HashMap<NK, NodeId, S>,
    edge_keys: HashMap<EK, crate::graph::EdgeId, S>,
}

impl<NK, EK, N, E, S> Default for KeyedGraph<NK, EK, N, E, S>
where
    NK: Eq + std::hash::Hash,
    EK: Eq + std::hash::Hash,
    S: BuildHasher + Default + Clone,
{
    fn default() -> Self {
        Self::with_hasher(S::default())
    }
}

/// A keyed graph using the default SipHash hasher.
impl<NK, EK, N, E> KeyedGraph<NK, EK, N, E>
where
    NK: Eq + std::hash::Hash,
    EK: Eq + std::hash::Hash,
{
    /// Create an empty keyed graph.
    pub fn new() -> Self {
        Self::with_hasher(std::collections::hash_map::RandomState::default())
    }
}

impl<NK, EK, N, E, S> KeyedGraph<NK, EK, N, E, S>
where
    NK: Eq + std::hash::Hash,
    EK: Eq + std::hash::Hash,
    S: BuildHasher + Default + Clone,
{
    /// Create an empty keyed graph with an explicit hasher.
    pub fn with_hasher(hasher: S) -> Self {
        Self {
            graph: Graph::new(),
            node_keys: HashMap::with_hasher(hasher.clone()),
            edge_keys: HashMap::with_hasher(hasher),
        }
    }

    /// The underlying logical graph.
    pub fn graph(&self) -> &Graph<N, E> {
        &self.graph
    }

    /// Resolve an external node key to an internal node identity.
    pub fn node_id(&self, key: &NK) -> Option<NodeId> {
        self.node_keys.get(key).copied()
    }

    /// Resolve an external edge key to an internal edge identity.
    pub fn edge_id(&self, key: &EK) -> Option<EdgeId> {
        self.edge_keys.get(key).copied()
    }

    /// Merge a batch of graph data into the graph (§8.1).
    ///
    /// Nodes already present are reused rather than duplicated. Edges whose
    /// endpoints cannot be resolved to a node are skipped.
    pub fn merge(&mut self, batch: GraphBatch<NK, EK, N, E>) -> GraphDelta {
        let mut delta = GraphDelta::default();

        for (key, data) in batch.nodes {
            self.upsert_node(key, data, &mut delta);
        }

        for (key, source_key, target_key, direction, data) in batch.edges {
            self.upsert_edge(key, &source_key, &target_key, direction, data, &mut delta);
        }

        delta
    }

    /// Apply a set of explicit mutations to the graph (§8.2).
    pub fn apply(&mut self, patch: GraphPatch<NK, EK, N, E>) -> GraphDelta {
        let mut delta = GraphDelta::default();

        for node_patch in patch.nodes {
            match node_patch {
                NodePatch::Upsert { key, data } => {
                    self.upsert_node(key, data, &mut delta);
                }
                NodePatch::Remove { key } => {
                    if let Some(id) = self.node_keys.remove(&key)
                        && let Some(d) = self.graph.remove_node(id)
                    {
                        // Drop edge-key entries for edges removed with the node.
                        for edge_id in &d.removed_edges {
                            self.edge_keys.retain(|_, v| v != edge_id);
                        }
                        delta.extend(d);
                    }
                }
            }
        }

        for edge_patch in patch.edges {
            match edge_patch {
                EdgePatch::Upsert {
                    key,
                    source,
                    target,
                    direction,
                    data,
                } => {
                    self.upsert_edge(key, &source, &target, direction, data, &mut delta);
                }
                EdgePatch::Remove { key } => {
                    if let Some(id) = self.edge_keys.remove(&key)
                        && let Some(d) = self.graph.remove_edge(id)
                    {
                        delta.extend(d);
                    }
                }
            }
        }

        delta
    }

    /// Upsert a node by external key, reusing an existing node or adding a new
    /// one (§8.1).
    fn upsert_node(&mut self, key: NK, data: N, delta: &mut GraphDelta) {
        if let Some(id) = self.node_keys.get(&key).copied() {
            if let Some(d) = self.graph.update_node_data(id, data) {
                delta.extend(d);
            }
        } else {
            let id = self.graph.add_node(data);
            self.node_keys.insert(key, id);
            delta.added_nodes.push(id);
            delta.topology_changed = true;
        }
    }

    /// Upsert an edge by external key, reusing an existing edge or adding a new
    /// one. Edges whose endpoints cannot be resolved to a node are skipped.
    fn upsert_edge(
        &mut self,
        key: EK,
        source_key: &NK,
        target_key: &NK,
        direction: EdgeDirection,
        data: E,
        delta: &mut GraphDelta,
    ) {
        let Some(source) = self.node_keys.get(source_key).copied() else {
            return;
        };
        let Some(target) = self.node_keys.get(target_key).copied() else {
            return;
        };
        if let Some(id) = self.edge_keys.get(&key).copied() {
            if let Some(d) = self.graph.update_edge(id, source, target, direction, data) {
                delta.extend(d);
            }
        } else if let Some(id) = self.graph.add_edge(source, target, direction, data) {
            self.edge_keys.insert(key, id);
            delta.added_edges.push(id);
            delta.topology_changed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EdgeDirection;
    use crate::interaction::{Hover, Selection};

    #[test]
    fn with_hasher_builds_key_maps_with_chosen_hasher() {
        let mut kg: KeyedGraph<&str, &str, (), (), rapidhash::fast::RandomState> =
            KeyedGraph::with_hasher(rapidhash::fast::RandomState::default());
        let delta = kg.merge(GraphBatch::new().node("a", ()).node("b", ()).edge(
            "ab",
            "a",
            "b",
            EdgeDirection::Directed,
            (),
        ));
        assert_eq!(delta.added_nodes.len(), 2);
        assert_eq!(delta.added_edges.len(), 1);
        assert_eq!(kg.node_id(&"a"), Some(kg.node_id(&"a").unwrap()));
        assert_eq!(kg.graph().node_count(), 2);
        assert_eq!(kg.graph().edge_count(), 1);
    }

    #[test]
    fn merge_reuses_existing_nodes() {
        let mut kg = KeyedGraph::new();
        let batch = GraphBatch::new()
            .node("alice", "Alice")
            .node("bob", "Bob")
            .edge("ab", "alice", "bob", EdgeDirection::Directed, "knows");
        let delta = kg.merge(batch);
        assert_eq!(delta.added_nodes.len(), 2);
        assert_eq!(delta.added_edges.len(), 1);

        // Merge a batch that reuses alice and adds carol.
        let batch2 = GraphBatch::new()
            .node("alice", "Alice")
            .node("carol", "Carol")
            .edge("ac", "alice", "carol", EdgeDirection::Directed, "knows");
        let delta2 = kg.merge(batch2);
        assert_eq!(delta2.added_nodes, vec![kg.node_id(&"carol").unwrap()]);
        // Re-merging an existing node re-provides its data, so it is reported
        // as updated rather than duplicated.
        assert_eq!(delta2.updated_nodes, vec![kg.node_id(&"alice").unwrap()]);
        assert_eq!(delta2.added_edges.len(), 1);

        assert_eq!(kg.graph().node_count(), 3);
        assert_eq!(kg.graph().edge_count(), 2);
        assert_eq!(kg.node_id(&"alice").unwrap(), kg.node_id(&"alice").unwrap());
    }

    #[test]
    fn merge_skips_edges_with_unknown_endpoints() {
        let mut kg = KeyedGraph::new();
        let batch = GraphBatch::new().node("alice", "Alice").edge(
            "ab",
            "alice",
            "ghost",
            EdgeDirection::Directed,
            "knows",
        );
        let delta = kg.merge(batch);
        assert_eq!(delta.added_nodes.len(), 1);
        assert_eq!(delta.added_edges.len(), 0);
        assert_eq!(kg.graph().edge_count(), 0);
    }

    #[test]
    fn apply_upsert_and_remove() {
        let mut kg = KeyedGraph::new();
        let delta = kg.apply(
            GraphPatch::new()
                .node(NodePatch::Upsert {
                    key: "a",
                    data: "A",
                })
                .node(NodePatch::Upsert {
                    key: "b",
                    data: "B",
                })
                .edge(EdgePatch::Upsert {
                    key: "ab",
                    source: "a",
                    target: "b",
                    direction: EdgeDirection::Undirected,
                    data: "link",
                }),
        );
        assert_eq!(delta.added_nodes.len(), 2);
        assert_eq!(delta.added_edges.len(), 1);

        // Remove node a; its incident edge must be removed too.
        let delta2 = kg.apply(GraphPatch::new().node(NodePatch::Remove { key: "a" }));
        assert_eq!(delta2.removed_nodes.len(), 1);
        assert_eq!(delta2.removed_edges.len(), 1);
        assert!(kg.node_id(&"a").is_none());
        assert!(kg.edge_id(&"ab").is_none());
        assert_eq!(kg.graph().node_count(), 1);
        assert_eq!(kg.graph().edge_count(), 0);
    }

    #[test]
    fn apply_upsert_updates_existing() {
        let mut kg: KeyedGraph<&str, &str, i32, ()> = KeyedGraph::new();
        kg.apply(GraphPatch::new().node(NodePatch::Upsert { key: "a", data: 1 }));
        let delta = kg.apply(GraphPatch::new().node(NodePatch::Upsert { key: "a", data: 2 }));
        assert_eq!(delta.updated_nodes.len(), 1);
        assert_eq!(delta.added_nodes.len(), 0);
        assert_eq!(kg.graph().node_data(kg.node_id(&"a").unwrap()), Some(&2));
    }

    #[test]
    fn edge_upsert_replaces_endpoints_direction_and_data_in_place() {
        let mut kg: KeyedGraph<&str, &str, (), &str> = KeyedGraph::new();
        kg.apply(
            GraphPatch::new()
                .node(NodePatch::Upsert { key: "a", data: () })
                .node(NodePatch::Upsert { key: "b", data: () })
                .node(NodePatch::Upsert { key: "c", data: () })
                .edge(EdgePatch::Upsert {
                    key: "edge",
                    source: "a",
                    target: "a",
                    direction: EdgeDirection::Directed,
                    data: "old",
                }),
        );
        let edge_id = kg.edge_id(&"edge").unwrap();
        let selection = Selection {
            nodes: Vec::new(),
            edges: vec![edge_id],
        };
        let hover = Hover {
            node: None,
            edge: Some(edge_id),
        };

        let delta = kg.apply(GraphPatch::new().edge(EdgePatch::Upsert {
            key: "edge",
            source: "b",
            target: "c",
            direction: EdgeDirection::Undirected,
            data: "new",
        }));
        assert_eq!(delta.updated_edges, vec![edge_id]);
        assert!(delta.topology_changed);
        assert_eq!(kg.edge_id(&"edge"), Some(edge_id));
        let edge = kg.graph().edge(edge_id).unwrap();
        assert_eq!(edge.source, kg.node_id(&"b").unwrap());
        assert_eq!(edge.target, kg.node_id(&"c").unwrap());
        assert_eq!(edge.direction, EdgeDirection::Undirected);
        assert_eq!(edge.data, "new");
        assert_eq!(selection.edges, vec![kg.edge_id(&"edge").unwrap()]);
        assert_eq!(hover.edge, kg.edge_id(&"edge"));
        assert!(kg.graph().edge(selection.edges[0]).is_some());
        assert_eq!(
            kg.graph().incident_edges(kg.node_id(&"a").unwrap()),
            Some(&[][..])
        );
        assert_eq!(
            kg.graph().incident_edges(kg.node_id(&"b").unwrap()),
            Some(&[edge_id][..])
        );
        assert_eq!(
            kg.graph().incident_edges(kg.node_id(&"c").unwrap()),
            Some(&[edge_id][..])
        );

        kg.apply(GraphPatch::new().edge(EdgePatch::Upsert {
            key: "edge",
            source: "c",
            target: "c",
            direction: EdgeDirection::Directed,
            data: "newer",
        }));
        assert_eq!(
            kg.graph().incident_edges(kg.node_id(&"b").unwrap()),
            Some(&[][..])
        );
        assert_eq!(
            kg.graph().incident_edges(kg.node_id(&"c").unwrap()),
            Some(&[edge_id][..])
        );
        assert_eq!(kg.edge_id(&"edge"), Some(edge_id));
    }

    #[test]
    fn edge_upsert_rejects_unknown_endpoint_without_partial_mutation() {
        let mut kg: KeyedGraph<&str, &str, (), &str> = KeyedGraph::new();
        kg.apply(
            GraphPatch::new()
                .node(NodePatch::Upsert { key: "a", data: () })
                .node(NodePatch::Upsert { key: "b", data: () })
                .edge(EdgePatch::Upsert {
                    key: "edge",
                    source: "a",
                    target: "b",
                    direction: EdgeDirection::Directed,
                    data: "old",
                }),
        );
        let edge_id = kg.edge_id(&"edge").unwrap();
        let topology_before = kg.graph().edge(edge_id).unwrap().clone();
        let source = kg.node_id(&"a").unwrap();
        let target = kg.node_id(&"b").unwrap();
        let source_incidence = kg.graph().incident_edges(source).unwrap().to_vec();
        let target_incidence = kg.graph().incident_edges(target).unwrap().to_vec();

        let delta = kg.apply(GraphPatch::new().edge(EdgePatch::Upsert {
            key: "edge",
            source: "a",
            target: "ghost",
            direction: EdgeDirection::Undirected,
            data: "new",
        }));
        assert!(delta.is_empty());
        assert_eq!(kg.edge_id(&"edge"), Some(edge_id));
        assert_eq!(
            kg.graph().edge(edge_id).unwrap().source,
            topology_before.source
        );
        assert_eq!(
            kg.graph().edge(edge_id).unwrap().target,
            topology_before.target
        );
        assert_eq!(
            kg.graph().edge(edge_id).unwrap().direction,
            topology_before.direction
        );
        assert_eq!(kg.graph().edge_data(edge_id), Some(&"old"));
        assert_eq!(kg.graph().incident_edges(source).unwrap(), source_incidence);
        assert_eq!(kg.graph().incident_edges(target).unwrap(), target_incidence);
    }

    #[test]
    fn edge_data_only_upsert_keeps_topology_delta_clear() {
        let mut kg: KeyedGraph<&str, &str, (), &str> = KeyedGraph::new();
        kg.apply(
            GraphPatch::new()
                .node(NodePatch::Upsert { key: "a", data: () })
                .node(NodePatch::Upsert { key: "b", data: () })
                .edge(EdgePatch::Upsert {
                    key: "edge",
                    source: "a",
                    target: "b",
                    direction: EdgeDirection::Directed,
                    data: "old",
                }),
        );
        let edge_id = kg.edge_id(&"edge").unwrap();
        let delta = kg.apply(GraphPatch::new().edge(EdgePatch::Upsert {
            key: "edge",
            source: "a",
            target: "b",
            direction: EdgeDirection::Directed,
            data: "new",
        }));

        assert_eq!(delta.updated_edges, vec![edge_id]);
        assert!(!delta.topology_changed);
        assert_eq!(
            kg.graph().incident_edges(kg.node_id(&"a").unwrap()),
            Some(&[edge_id][..])
        );
        assert_eq!(kg.graph().edge_data(edge_id), Some(&"new"));
    }

    #[test]
    fn removing_and_reinserting_edge_keeps_key_map_consistent() {
        let mut kg: KeyedGraph<&str, &str, (), ()> = KeyedGraph::new();
        kg.apply(
            GraphPatch::new()
                .node(NodePatch::Upsert { key: "a", data: () })
                .node(NodePatch::Upsert { key: "b", data: () })
                .edge(EdgePatch::Upsert {
                    key: "edge",
                    source: "a",
                    target: "b",
                    direction: EdgeDirection::Directed,
                    data: (),
                }),
        );
        let old_id = kg.edge_id(&"edge").unwrap();

        let removed = kg.apply(GraphPatch::new().edge(EdgePatch::Remove { key: "edge" }));
        assert_eq!(removed.removed_edges, vec![old_id]);
        assert!(removed.topology_changed);
        assert!(kg.edge_id(&"edge").is_none());
        assert_eq!(
            kg.graph().incident_edges(kg.node_id(&"a").unwrap()),
            Some(&[][..])
        );
        assert_eq!(
            kg.graph().incident_edges(kg.node_id(&"b").unwrap()),
            Some(&[][..])
        );

        let inserted = kg.apply(GraphPatch::new().edge(EdgePatch::Upsert {
            key: "edge",
            source: "a",
            target: "b",
            direction: EdgeDirection::Undirected,
            data: (),
        }));
        let new_id = kg.edge_id(&"edge").unwrap();
        assert_ne!(new_id, old_id);
        assert_eq!(inserted.added_edges, vec![new_id]);
        assert_eq!(
            kg.graph().incident_edges(kg.node_id(&"a").unwrap()),
            Some(&[new_id][..])
        );
        assert_eq!(
            kg.graph().incident_edges(kg.node_id(&"b").unwrap()),
            Some(&[new_id][..])
        );
    }
}
