//! Graph update primitives: batches and patches.
//!
//! A [`GraphBatch`] represents graph data to be merged into the currently known
//! graph (typically a database query result). A [`GraphPatch`] represents
//! explicit mutations. Both are expressed using external application keys and
//! are applied through a [`KeyedGraph`](crate::keyed_graph::KeyedGraph), which
//! translates them into internal identities and produces a
//! [`GraphDelta`](crate::graph::GraphDelta) (§8).
//!
//! An existing edge key is updated in place: its stable `EdgeId` is retained,
//! while endpoints, direction, and data are replaced together. Endpoint
//! validation and adjacency maintenance belong to `Graph`, not to patch
//! callers.

use crate::graph::EdgeDirection;

/// A batch of graph data to merge into a graph.
///
/// Nodes already present in the graph are reused rather than duplicated (§8.1).
#[derive(Debug, Clone)]
pub struct GraphBatch<NK, EK, N, E> {
    /// Nodes to upsert, keyed by external node key.
    pub nodes: Vec<(NK, N)>,
    /// Edges to upsert, keyed by external edge key and referencing node keys.
    pub edges: Vec<(EK, NK, NK, EdgeDirection, E)>,
}

impl<NK, EK, N, E> Default for GraphBatch<NK, EK, N, E> {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }
}

impl<NK, EK, N, E> GraphBatch<NK, EK, N, E> {
    /// Create an empty batch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a node to the batch.
    pub fn node(mut self, key: NK, data: N) -> Self {
        self.nodes.push((key, data));
        self
    }

    /// Add an edge to the batch.
    pub fn edge(
        mut self,
        key: EK,
        source: NK,
        target: NK,
        direction: EdgeDirection,
        data: E,
    ) -> Self {
        self.edges.push((key, source, target, direction, data));
        self
    }
}

/// An explicit node mutation, keyed by external node key (§8.2).
#[derive(Debug, Clone)]
pub enum NodePatch<NK, N> {
    /// Insert or replace the node with the given key and data.
    Upsert { key: NK, data: N },
    /// Remove the node with the given key.
    Remove { key: NK },
}

/// An explicit edge mutation, keyed by external edge key (§8.2).
#[derive(Debug, Clone)]
pub enum EdgePatch<NK, EK, E> {
    /// Insert or replace the edge with the given key.
    Upsert {
        key: EK,
        source: NK,
        target: NK,
        direction: EdgeDirection,
        data: E,
    },
    /// Remove the edge with the given key.
    Remove { key: EK },
}

/// A set of explicit graph mutations (§8.2).
#[derive(Debug, Clone)]
pub struct GraphPatch<NK, EK, N, E> {
    /// Node mutations.
    pub nodes: Vec<NodePatch<NK, N>>,
    /// Edge mutations.
    pub edges: Vec<EdgePatch<NK, EK, E>>,
}

impl<NK, EK, N, E> Default for GraphPatch<NK, EK, N, E> {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }
}

impl<NK, EK, N, E> GraphPatch<NK, EK, N, E> {
    /// Create an empty patch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a node mutation.
    pub fn node(mut self, patch: NodePatch<NK, N>) -> Self {
        self.nodes.push(patch);
        self
    }

    /// Add an edge mutation.
    pub fn edge(mut self, patch: EdgePatch<NK, EK, E>) -> Self {
        self.edges.push(patch);
        self
    }
}
