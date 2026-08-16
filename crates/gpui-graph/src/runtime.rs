//! Graph runtime (§20).
//!
//! Rendering caches and spatial acceleration live outside the logical graph.
//! The runtime is derived state: it must be reconstructible from authoritative
//! graph and scene state. v0.1 keeps the runtime minimal and tracks revisions
//! so expensive derived structures can be invalidated selectively (§31).

use crate::graph::{EdgeId, NodeId};

/// Derived rendering state for a graph scene (§20).
///
/// v0.1 holds revision bookkeeping and a small cache of node positions keyed by
/// stable identity. Spatial indexing and geometry caches are deferred until
/// profiling justifies them (§37).
#[derive(Debug, Clone, Default)]
pub struct GraphRuntime {
    /// The topology revision this runtime was synced to.
    topology_revision: u64,
    /// The geometry revision this runtime was synced to.
    geometry_revision: u64,
    /// Cached node positions, keyed by node identity.
    node_positions: std::collections::HashMap<NodeId, glam::Vec2>,
}

impl GraphRuntime {
    /// Create an empty runtime.
    pub fn new() -> Self {
        Self::default()
    }

    /// The topology revision this runtime was synced to.
    pub fn topology_revision(&self) -> u64 {
        self.topology_revision
    }

    /// The geometry revision this runtime was synced to.
    pub fn geometry_revision(&self) -> u64 {
        self.geometry_revision
    }

    /// Whether the runtime is stale relative to the given revisions.
    pub fn is_stale(&self, topology_revision: u64, geometry_revision: u64) -> bool {
        self.topology_revision != topology_revision || self.geometry_revision != geometry_revision
    }

    /// Sync the runtime to the given revisions, clearing derived caches when
    /// the topology changed.
    pub fn sync(&mut self, topology_revision: u64, geometry_revision: u64) {
        if self.topology_revision != topology_revision {
            self.node_positions.clear();
        }
        self.topology_revision = topology_revision;
        self.geometry_revision = geometry_revision;
    }

    /// Cache a node's position.
    pub fn set_node_position(&mut self, node: NodeId, position: glam::Vec2) {
        self.node_positions.insert(node, position);
    }

    /// Look up a cached node position.
    pub fn node_position(&self, node: NodeId) -> Option<glam::Vec2> {
        self.node_positions.get(&node).copied()
    }

    /// Remove a node from the cache.
    pub fn remove_node(&mut self, node: NodeId) {
        self.node_positions.remove(&node);
    }

    /// Remove an edge from any caches (no-op in v0.1, kept for symmetry).
    pub fn remove_edge(&mut self, _edge: EdgeId) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_clears_cache_on_topology_change() {
        let mut rt = GraphRuntime::new();
        let node = NodeId::default();
        rt.set_node_position(node, glam::Vec2::new(1.0, 2.0));
        rt.sync(0, 0);
        assert_eq!(rt.node_position(node), Some(glam::Vec2::new(1.0, 2.0)));

        // Topology change clears the cache.
        rt.sync(1, 0);
        assert_eq!(rt.node_position(node), None);
    }

    #[test]
    fn geometry_change_does_not_clear_cache() {
        let mut rt = GraphRuntime::new();
        let node = NodeId::default();
        rt.set_node_position(node, glam::Vec2::new(1.0, 2.0));
        rt.sync(0, 0);
        rt.sync(0, 1);
        assert_eq!(rt.node_position(node), Some(glam::Vec2::new(1.0, 2.0)));
    }

    #[test]
    fn is_stale_detects_revision_mismatch() {
        let mut rt = GraphRuntime::new();
        rt.sync(1, 2);
        assert!(!rt.is_stale(1, 2));
        assert!(rt.is_stale(2, 2));
        assert!(rt.is_stale(1, 3));
    }
}
