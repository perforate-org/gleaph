//! Dense layout projection.
//!
//! The logical graph is projected into a dense representation optimized for
//! numerical layout algorithms (§11.2). Stable graph identity is deliberately
//! separated from dense indexing: layout hot loops operate over dense data
//! (Invariant 4) while graph state keeps stable generational identity.

use bitvec::prelude::*;
use glam::Vec2;

use crate::graph::{EdgeDirection, NodeId};

/// A dense index into layout arrays.
///
/// This allows layout hot loops to use `positions[index.0 as usize]` instead of
/// hash or generational-key lookups (§11.2).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LayoutIndex(pub u32);

/// A node in the dense layout projection.
///
/// Kept as a distinct type so future per-node layout attributes have a home;
/// currently carries no data (pinning lives in [`LayoutState`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutNode {}

/// An edge in the dense layout projection.
#[derive(Debug, Clone, Copy)]
pub struct LayoutEdge {
    /// Dense index of the source node.
    pub source: LayoutIndex,
    /// Dense index of the target node.
    pub target: LayoutIndex,
    /// Directionality of the edge.
    pub direction: EdgeDirection,
}

/// A dense projection of a graph for numerical layout (§11.2).
#[derive(Debug, Clone)]
pub struct LayoutGraph {
    /// Dense node records, parallel to `node_ids` and `state.positions`.
    pub nodes: Vec<LayoutNode>,
    /// Dense edge records.
    pub edges: Vec<LayoutEdge>,
    /// Stable graph identity for each dense node slot.
    pub node_ids: Vec<NodeId>,
    /// Incremented whenever the topology of this projection changes.
    pub topology_revision: u64,
}

impl LayoutGraph {
    /// Number of nodes in the projection.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges in the projection.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
}

/// Algorithm-independent shared layout state (§11.4).
///
/// Algorithm-specific state (velocity, temperature, Barnes-Hut tree, ...) does
/// not belong here; it lives inside the layout engine.
#[derive(Debug, Clone)]
pub struct LayoutState {
    /// Dense positions, parallel to `LayoutGraph::node_ids`.
    pub positions: Vec<Vec2>,
    /// Whether each dense node is hard-pinned.
    pub pinned: BitVec,
}

impl LayoutState {
    /// Create empty layout state.
    pub fn new() -> Self {
        Self {
            positions: Vec::new(),
            pinned: BitVec::new(),
        }
    }

    /// Resize the state to hold `len` nodes, preserving existing values.
    pub fn resize(&mut self, len: usize) {
        self.positions.resize(len, Vec2::ZERO);
        self.pinned.resize(len, false);
    }

    /// Position of a dense node.
    pub fn position(&self, index: LayoutIndex) -> Vec2 {
        self.positions[index.0 as usize]
    }

    /// Whether a dense node is pinned.
    pub fn is_pinned(&self, index: LayoutIndex) -> bool {
        self.pinned[index.0 as usize]
    }
}

impl Default for LayoutState {
    fn default() -> Self {
        Self::new()
    }
}
