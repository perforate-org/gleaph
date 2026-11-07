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
///
/// Construct through [`LayoutGraph::new`], which derives the per-node
/// [`Adjacency`] views from the edge list. Mutating `edges` after
/// construction leaves those views stale, so treat the projection as
/// rebuilt-wholesale on topology changes — exactly what `GraphScene` does.
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
    /// Per-node neighbor views derived from `edges` at construction.
    adjacency: Adjacency,
}

impl LayoutGraph {
    /// Build a dense projection and derive its adjacency views.
    pub fn new(
        nodes: Vec<LayoutNode>,
        edges: Vec<LayoutEdge>,
        node_ids: Vec<NodeId>,
        topology_revision: u64,
    ) -> Self {
        let adjacency = Adjacency::build(nodes.len(), &edges, AdjacencyMode::Incident);
        Self {
            nodes,
            edges,
            node_ids,
            topology_revision,
            adjacency,
        }
    }

    /// Number of nodes in the projection.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges in the projection.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// The undirected incidence view: every edge contributes both endpoints,
    /// regardless of [`EdgeDirection`] (§15.1 attraction physics).
    pub fn adjacency(&self) -> &Adjacency {
        &self.adjacency
    }
}

/// Which neighbor semantics an [`Adjacency`] view encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjacencyMode {
    /// Every edge contributes both endpoints exactly once per endpoint,
    /// regardless of direction. Duplicate edges contribute once per duplicate;
    /// a self-loop contributes its single node twice. This is what force-based
    /// layouts need for per-node gathering.
    Incident,
    /// [`EdgeDirection::Directed`] contributes only `source → target`;
    /// [`EdgeDirection::Undirected`] contributes both ways. This is what
    /// reachability algorithms such as Tarjan's SCC need.
    Successors,
}

/// A CSR-style per-node neighbor view over the dense edge list.
///
/// One owner of the construction invariant (`offsets` is monotonic with length
/// `n + 1`; `targets[offsets[i]..offsets[i + 1]]` holds node `i`'s neighbors),
/// so engines gather incident data through slices instead of each rebuilding
/// bespoke adjacency (§11.2).
#[derive(Debug, Clone, Default)]
pub struct Adjacency {
    offsets: Vec<u32>,
    targets: Vec<u32>,
}

impl Adjacency {
    /// Build `node_count` rows from `edges` under `mode`.
    ///
    /// Neighbor lists preserve edge iteration order, so construction is
    /// deterministic.
    pub fn build(node_count: usize, edges: &[LayoutEdge], mode: AdjacencyMode) -> Self {
        let mut offsets = vec![0u32; node_count + 1];
        let mut degree = |i: usize| offsets[i + 1] += 1;
        for edge in edges {
            let source = edge.source.0 as usize;
            let target = edge.target.0 as usize;
            match mode {
                AdjacencyMode::Incident => {
                    degree(source);
                    degree(target);
                }
                AdjacencyMode::Successors => {
                    degree(source);
                    if edge.direction == EdgeDirection::Undirected {
                        degree(target);
                    }
                }
            }
        }
        for window in 1..=node_count {
            offsets[window] += offsets[window - 1];
        }
        let mut targets = vec![0u32; offsets[node_count] as usize];
        let mut cursor = offsets[..node_count].to_vec();
        let mut place = |from: usize, to: u32, cursor: &mut [u32]| {
            targets[cursor[from] as usize] = to;
            cursor[from] += 1;
        };
        for edge in edges {
            let source = edge.source.0 as usize;
            let target = edge.target.0 as usize;
            match mode {
                AdjacencyMode::Incident => {
                    place(source, edge.target.0, &mut cursor);
                    place(target, edge.source.0, &mut cursor);
                }
                AdjacencyMode::Successors => {
                    place(source, edge.target.0, &mut cursor);
                    if edge.direction == EdgeDirection::Undirected {
                        place(target, edge.source.0, &mut cursor);
                    }
                }
            }
        }
        Self { offsets, targets }
    }

    /// Neighbors of dense node `i` under this view's [`AdjacencyMode`].
    pub fn neighbors(&self, i: LayoutIndex) -> &[u32] {
        let lo = self.offsets[i.0 as usize] as usize;
        let hi = self.offsets[i.0 as usize + 1] as usize;
        &self.targets[lo..hi]
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
    /// Optional cluster center and radius for each dense node, in world
    /// coordinates.
    ///
    /// A layout engine that groups nodes (e.g. an SCC circular layout) records
    /// the center and radius of each node's cluster here so the paint layer can
    /// bow edges within a cluster outward from its center, keeping the cluster's
    /// circular shape readable even when node spacing is large. Nodes without a
    /// cluster carry `None`.
    pub cluster_centers: Vec<Option<(Vec2, f32)>>,
}

impl LayoutState {
    /// Create empty layout state.
    pub fn new() -> Self {
        Self {
            positions: Vec::new(),
            pinned: BitVec::new(),
            cluster_centers: Vec::new(),
        }
    }

    /// Resize the state to hold `len` nodes, preserving existing values.
    pub fn resize(&mut self, len: usize) {
        self.positions.resize(len, Vec2::ZERO);
        self.pinned.resize(len, false);
        self.cluster_centers.resize(len, None);
    }

    /// Position of a dense node.
    pub fn position(&self, index: LayoutIndex) -> Vec2 {
        self.positions[index.0 as usize]
    }

    /// Whether a dense node is pinned.
    pub fn is_pinned(&self, index: LayoutIndex) -> bool {
        self.pinned[index.0 as usize]
    }

    /// Cluster center and radius of a dense node, if it belongs to a cluster.
    pub fn cluster_center(&self, index: LayoutIndex) -> Option<(Vec2, f32)> {
        self.cluster_centers[index.0 as usize]
    }
}

impl Default for LayoutState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EdgeDirection;

    fn edges(pairs: &[(u32, u32, EdgeDirection)]) -> Vec<LayoutEdge> {
        pairs
            .iter()
            .map(|&(s, t, d)| LayoutEdge {
                source: LayoutIndex(s),
                target: LayoutIndex(t),
                direction: d,
            })
            .collect()
    }

    #[test]
    fn incident_view_lists_both_endpoints_per_edge() {
        // Parallel edges contribute twice, a self-loop contributes two
        // zero-length entries, and every undirected edge appears under both
        // ends — the exact multiset attraction gathering consumes.
        let pairs = [
            (0u32, 1u32, EdgeDirection::Undirected),
            (0, 1, EdgeDirection::Undirected),
            (1, 2, EdgeDirection::Undirected),
            (2, 2, EdgeDirection::Undirected),
        ];
        let view = Adjacency::build(3, &edges(&pairs), AdjacencyMode::Incident);
        assert_eq!(view.neighbors(LayoutIndex(0)), &[1, 1]);
        assert_eq!(view.neighbors(LayoutIndex(1)), &[0, 0, 2]);
        assert_eq!(view.neighbors(LayoutIndex(2)), &[1, 2, 2]);
    }

    #[test]
    fn successors_view_respects_direction() {
        // Directed edges flow one way only; undirected edges flow both ways.
        let pairs = [
            (0u32, 1u32, EdgeDirection::Directed),
            (1, 2, EdgeDirection::Undirected),
        ];
        let view = Adjacency::build(3, &edges(&pairs), AdjacencyMode::Successors);
        assert_eq!(view.neighbors(LayoutIndex(0)), &[1]);
        assert_eq!(view.neighbors(LayoutIndex(1)), &[2]);
        assert_eq!(view.neighbors(LayoutIndex(2)), &[1]);
    }

    #[test]
    fn empty_graph_yields_empty_rows() {
        let view = Adjacency::build(4, &[], AdjacencyMode::Incident);
        for i in 0..4 {
            assert!(view.neighbors(LayoutIndex(i)).is_empty());
        }
        let graph = LayoutGraph::new(vec![], vec![], vec![], 0);
        assert_eq!(graph.node_count(), 0);
    }
}
