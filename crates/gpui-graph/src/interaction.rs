//! Interaction state and events (§23, §24).
//!
//! v0.1 supports hover, node/edge selection, pan, zoom, node drag, pin, and
//! unpin. Graph-database-specific actions such as "expand neighbors" do not
//! belong in `gpui-graph`; the component emits general interaction events that
//! a higher-level application interprets (Invariant 8).

use crate::graph::{EdgeId, NodeId};

/// The current selection (§25).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Selected nodes.
    pub nodes: Vec<NodeId>,
    /// Selected edges.
    pub edges: Vec<EdgeId>,
}

impl Selection {
    /// Create an empty selection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the selection is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }

    /// Whether a node is selected.
    pub fn contains_node(&self, node: NodeId) -> bool {
        self.nodes.contains(&node)
    }

    /// Whether an edge is selected.
    pub fn contains_edge(&self, edge: EdgeId) -> bool {
        self.edges.contains(&edge)
    }
}

/// The current hover target (§23).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hover {
    /// The hovered node, if any.
    pub node: Option<NodeId>,
    /// The hovered edge, if any.
    pub edge: Option<EdgeId>,
}

/// General graph interaction events (§24).
///
/// A higher-level application interprets these events; `gpui-graph` does not
/// assign graph-database semantics to them.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphEvent {
    /// A node was clicked.
    NodeClicked {
        /// The clicked node.
        node: NodeId,
        /// The mouse button.
        button: MouseButton,
    },
    /// A node was double-clicked.
    NodeDoubleClicked {
        /// The double-clicked node.
        node: NodeId,
    },
    /// An edge was clicked.
    EdgeClicked {
        /// The clicked edge.
        edge: EdgeId,
        /// The mouse button.
        button: MouseButton,
    },
    /// The selection changed.
    SelectionChanged {
        /// The new selection.
        selection: Selection,
    },
    /// A node was moved (e.g. by dragging).
    NodeMoved {
        /// The moved node.
        node: NodeId,
        /// The new world-space position.
        position: glam::Vec2,
    },
    /// The viewport changed (pan, zoom, or resize).
    ViewportChanged,
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    /// The left button.
    Left,
    /// The right button.
    Right,
    /// The middle button.
    Middle,
}
