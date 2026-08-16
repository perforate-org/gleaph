//! Paint frame (§18.2).
//!
//! A [`PaintFrame`] is an intermediate frame representation containing only the
//! information required for the current paint: visible nodes and edges already
//! transformed to canvas-local pixels, plus interaction highlights. This
//! separates graph and scene state from rendering mechanics. The `GraphView`
//! boundary owns the separate conversion between canvas-local and window-space
//! GPUI coordinates.

use glam::Vec2;

use crate::graph::{EdgeDirection, EdgeId, Graph, NodeId};
use crate::interaction::{Hover, Selection};
use crate::style::GraphStyle;
use crate::viewport::Viewport;

/// A node record ready for painting.
#[derive(Debug, Clone, Copy)]
pub struct PaintNode {
    /// Stable node identity.
    pub id: NodeId,
    /// Canvas-local pixel position.
    pub position: Vec2,
    /// Node radius in pixels.
    pub radius: f32,
    /// Whether the node is selected.
    pub selected: bool,
    /// Whether the node is hovered.
    pub hovered: bool,
}

/// An edge record ready for painting.
#[derive(Debug, Clone, Copy)]
pub struct PaintEdge {
    /// Stable edge identity.
    pub id: EdgeId,
    /// Canvas-local pixel source position.
    pub source: Vec2,
    /// Canvas-local pixel target position.
    pub target: Vec2,
    /// Whether the edge is directed.
    pub direction: EdgeDirection,
    /// Whether the edge is selected.
    pub selected: bool,
    /// Whether the edge is hovered.
    pub hovered: bool,
}

/// A label record ready for painting.
#[derive(Debug, Clone)]
pub struct PaintLabel {
    /// Canvas-local pixel anchor position (the node center).
    pub position: Vec2,
    /// The label text.
    pub text: String,
}

/// An edge label record ready for painting.
#[derive(Debug, Clone)]
pub struct PaintEdgeLabel {
    /// Canvas-local pixel anchor position (the edge midpoint).
    pub position: Vec2,
    /// The label text.
    pub text: String,
}

/// The set of primitives to paint for one frame (§18.2).
#[derive(Debug, Clone, Default)]
pub struct PaintFrame {
    /// Visible nodes.
    pub nodes: Vec<PaintNode>,
    /// Visible edges.
    pub edges: Vec<PaintEdge>,
    /// Visible node labels.
    pub labels: Vec<PaintLabel>,
    /// Visible edge labels.
    pub edge_labels: Vec<PaintEdgeLabel>,
}

impl PaintFrame {
    /// Create an empty paint frame.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the frame contains no primitives.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
            && self.edges.is_empty()
            && self.labels.is_empty()
            && self.edge_labels.is_empty()
    }
}

/// Inputs to [`build_paint_frame`].
pub struct PaintFrameInput<'a, N, E> {
    /// The logical graph.
    pub graph: &'a Graph<N, E>,
    /// Resolves a node's world-space position.
    pub node_position: &'a dyn Fn(NodeId) -> Option<Vec2>,
    /// Resolves an optional node label string.
    pub node_label: &'a dyn Fn(NodeId, &N) -> Option<String>,
    /// Resolves an optional edge label string.
    pub edge_label: &'a dyn Fn(EdgeId, &E) -> Option<String>,
    /// The viewport.
    pub viewport: &'a Viewport,
    /// The graph style.
    pub style: &'a GraphStyle,
    /// The current selection.
    pub selection: &'a Selection,
    /// The current hover target.
    pub hover: &'a Hover,
}

/// Build a paint frame from graph, scene positions, viewport, style, and
/// interaction state, applying viewport culling (§22).
///
/// `node_position` resolves a node's world-space position (typically from the
/// scene's node scene state). `node_label` resolves an optional label string for
/// a node; nodes without a label produce no label primitive. `edge_label`
/// resolves an optional label string for an edge; edges without a label produce
/// no edge-label primitive.
pub fn build_paint_frame<N, E>(input: PaintFrameInput<'_, N, E>) -> PaintFrame {
    let PaintFrameInput {
        graph,
        node_position,
        node_label,
        edge_label,
        viewport,
        style,
        selection,
        hover,
    } = input;
    let visible = viewport.visible_world_bounds();
    let margin = style.node_radius * 2.0;

    let mut frame = PaintFrame::new();

    // A degenerate (zero-area) viewport has nothing visible.
    if visible.is_empty() {
        return frame;
    }

    for (id, node) in graph.nodes() {
        let Some(world) = node_position(id) else {
            continue;
        };
        // Cull nodes outside the visible world bounds (with margin).
        if world.x < visible.min.x - margin
            || world.x > visible.max.x + margin
            || world.y < visible.min.y - margin
            || world.y > visible.max.y + margin
        {
            continue;
        }
        frame.nodes.push(PaintNode {
            id,
            position: viewport.world_to_screen(world),
            radius: style.node_radius,
            selected: selection.contains_node(id),
            hovered: hover.node == Some(id),
        });
        if let Some(text) = node_label(id, &node.data) {
            frame.labels.push(PaintLabel {
                position: viewport.world_to_screen(world),
                text,
            });
        }
    }

    for (id, edge) in graph.edges() {
        let Some(source_world) = node_position(edge.source) else {
            continue;
        };
        let Some(target_world) = node_position(edge.target) else {
            continue;
        };
        // Cull edges whose endpoints are both outside the visible bounds.
        let source_visible = point_in_bounds(source_world, &visible, margin);
        let target_visible = point_in_bounds(target_world, &visible, margin);
        if !source_visible && !target_visible {
            continue;
        }
        frame.edges.push(PaintEdge {
            id,
            source: viewport.world_to_screen(source_world),
            target: viewport.world_to_screen(target_world),
            direction: edge.direction,
            selected: selection.contains_edge(id),
            hovered: hover.edge == Some(id),
        });
        if let Some(text) = edge_label(id, &edge.data) {
            let source_screen = viewport.world_to_screen(source_world);
            let target_screen = viewport.world_to_screen(target_world);
            frame.edge_labels.push(PaintEdgeLabel {
                position: (source_screen + target_screen) * 0.5,
                text,
            });
        }
    }

    frame
}

fn point_in_bounds(p: Vec2, bounds: &crate::viewport::WorldBounds, margin: f32) -> bool {
    p.x >= bounds.min.x - margin
        && p.x <= bounds.max.x + margin
        && p.y >= bounds.min.y - margin
        && p.y <= bounds.max.y + margin
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{EdgeDirection, Graph};
    use crate::viewport::WorldBounds;

    fn graph() -> Graph<(), ()> {
        let mut g = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Directed, ());
        g
    }

    fn positions() -> impl Fn(NodeId) -> Option<Vec2> {
        |_id| Some(Vec2::ZERO)
    }

    fn no_labels<N>() -> impl Fn(NodeId, &N) -> Option<String> {
        |_id, _node| None
    }

    fn no_edge_labels<E>() -> impl Fn(EdgeId, &E) -> Option<String> {
        |_id, _edge| None
    }

    #[test]
    fn culls_nodes_outside_viewport() {
        let g = graph();
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(100.0, 100.0));
        vp.fit_bounds(
            WorldBounds {
                min: Vec2::new(-10.0, -10.0),
                max: Vec2::new(10.0, 10.0),
            },
            0.0,
        );
        let style = GraphStyle::default();
        let selection = Selection::new();
        let hover = Hover::default();

        // Node `a` is inside the viewport; node `b` is far outside.
        let mut it = g.nodes();
        let a = it.next().unwrap().0;
        let b = it.next().unwrap().0;
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::ZERO)
            } else if id == b {
                Some(Vec2::new(1000.0, 1000.0))
            } else {
                None
            }
        };

        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &style,
            selection: &selection,
            hover: &hover,
        });
        // Only the in-viewport node is painted; the out-of-viewport node is culled.
        assert_eq!(frame.nodes.len(), 1);
        assert_eq!(frame.nodes[0].id, a);
        // The edge is kept because one endpoint (`a`) is visible.
        assert_eq!(frame.edges.len(), 1);
    }

    #[test]
    fn empty_viewport_culls_everything() {
        let g = graph();
        let vp = Viewport::new(); // size zero
        let style = GraphStyle::default();
        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions(),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &style,
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        assert!(frame.is_empty());
    }

    #[test]
    fn marks_selected_and_hovered() {
        let g = graph();
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(100.0, 100.0));
        let style = GraphStyle::default();
        let node = g.nodes().next().unwrap().0;
        let mut selection = Selection::new();
        selection.nodes.push(node);
        let hover = Hover {
            node: Some(node),
            edge: None,
        };
        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions(),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &style,
            selection: &selection,
            hover: &hover,
        });
        let painted = frame.nodes.iter().find(|n| n.id == node).unwrap();
        assert!(painted.selected);
        assert!(painted.hovered);
    }

    #[test]
    fn geometry_remains_canvas_local() {
        let mut graph = Graph::new();
        let source = graph.add_node(());
        let target = graph.add_node(());
        graph.add_edge(source, target, EdgeDirection::Directed, ());
        let positions = move |id: NodeId| {
            if id == source {
                Some(Vec2::new(-10.0, 0.0))
            } else if id == target {
                Some(Vec2::new(10.0, 0.0))
            } else {
                None
            }
        };
        let mut viewport = Viewport::new();
        viewport.set_size(Vec2::new(100.0, 100.0));
        let frame = build_paint_frame(PaintFrameInput {
            graph: &graph,
            node_position: &positions,
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &viewport,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });

        let source_node = frame.nodes.iter().find(|node| node.id == source).unwrap();
        let target_node = frame.nodes.iter().find(|node| node.id == target).unwrap();
        assert_eq!(source_node.position, Vec2::new(40.0, 50.0));
        assert_eq!(target_node.position, Vec2::new(60.0, 50.0));
        assert_eq!(frame.edges[0].source, Vec2::new(40.0, 50.0));
        assert_eq!(frame.edges[0].target, Vec2::new(60.0, 50.0));
    }

    #[test]
    fn produces_labels_for_nodes_with_text() {
        let mut g: Graph<&str, ()> = Graph::new();
        let a = g.add_node("alice");
        let b = g.add_node("bob");
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else if id == b {
                Some(Vec2::new(10.0, 10.0))
            } else {
                None
            }
        };
        let labels = move |_id: NodeId, node: &&str| Some(node.to_string());
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(100.0, 100.0));
        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_label: &labels,
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        assert_eq!(frame.labels.len(), 2);
        assert_eq!(frame.labels[0].text, "alice");
        assert_eq!(frame.labels[1].text, "bob");
    }

    #[test]
    fn produces_edge_labels_at_midpoint() {
        let mut g: Graph<(), &str> = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Directed, "knows");
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else if id == b {
                Some(Vec2::new(10.0, 0.0))
            } else {
                None
            }
        };
        let edge_labels = move |_id: EdgeId, edge: &&str| Some(edge.to_string());
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(100.0, 100.0));
        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_label: &no_labels(),
            edge_label: &edge_labels,
            viewport: &vp,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        assert_eq!(frame.edge_labels.len(), 1);
        assert_eq!(frame.edge_labels[0].text, "knows");
        assert_eq!(frame.edge_labels[0].position, Vec2::new(55.0, 50.0));
    }
}
