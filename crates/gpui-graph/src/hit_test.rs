//! Hit testing (§21).
//!
//! Hit testing uses two conceptual stages: coarse candidate lookup followed by
//! a precise geometry test. v0.1 uses a direct scan for both stages; the exact
//! spatial index is deliberately not fixed and should be selected empirically
//! (§21, §37).

use glam::Vec2;

use crate::graph::{EdgeId, Graph, NodeId};
use crate::style::GraphStyle;
use crate::viewport::Viewport;

/// The result of a hit test.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HitTestResult {
    /// The hit node, if any.
    pub node: Option<NodeId>,
    /// The hit edge, if any.
    pub edge: Option<EdgeId>,
}

impl HitTestResult {
    /// Whether any primitive was hit.
    pub fn is_hit(&self) -> bool {
        self.node.is_some() || self.edge.is_some()
    }
}

/// Hit-test a screen point against the graph's nodes and edges.
///
/// Nodes are tested by distance to their screen-space center within the node
/// radius. Edges are tested by distance from the point to the screen-space
/// segment. Nodes take precedence over edges when both are hit.
pub fn hit_test<N, E>(
    graph: &Graph<N, E>,
    node_position: &impl Fn(NodeId) -> Option<Vec2>,
    viewport: &Viewport,
    style: &GraphStyle,
    screen_point: Vec2,
) -> HitTestResult {
    // Precise node test.
    let mut best_node: Option<(NodeId, f32)> = None;
    for (id, _) in graph.nodes() {
        let Some(world) = node_position(id) else {
            continue;
        };
        let screen = viewport.world_to_screen(world);
        let dist = (screen - screen_point).length();
        if dist <= style.node_radius && best_node.is_none_or(|(_, d)| dist < d) {
            best_node = Some((id, dist));
        }
    }
    if let Some((node, _)) = best_node {
        return HitTestResult {
            node: Some(node),
            edge: None,
        };
    }

    // Precise edge test.
    let mut best_edge: Option<(EdgeId, f32)> = None;
    for (id, edge) in graph.edges() {
        let Some(source_world) = node_position(edge.source) else {
            continue;
        };
        let Some(target_world) = node_position(edge.target) else {
            continue;
        };
        let source = viewport.world_to_screen(source_world);
        let target = viewport.world_to_screen(target_world);
        let dist = distance_to_segment(screen_point, source, target);
        let threshold = (style.edge_width * 0.5 + 2.0).max(3.0);
        if dist <= threshold && best_edge.is_none_or(|(_, d)| dist < d) {
            best_edge = Some((id, dist));
        }
    }

    HitTestResult {
        node: None,
        edge: best_edge.map(|(e, _)| e),
    }
}

/// Distance from a point to a line segment.
fn distance_to_segment(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let ab = b - a;
    let len_sq = ab.length_squared();
    if len_sq == 0.0 {
        return (p - a).length();
    }
    let t = ((p - a).dot(ab) / len_sq).clamp(0.0, 1.0);
    (p - (a + ab * t)).length()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{EdgeDirection, Graph};
    use crate::viewport::WorldBounds;

    fn setup() -> (
        Graph<(), ()>,
        impl Fn(NodeId) -> Option<Vec2>,
        Viewport,
        GraphStyle,
    ) {
        let mut g = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Directed, ());
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else if id == b {
                Some(Vec2::new(100.0, 0.0))
            } else {
                None
            }
        };
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(200.0, 200.0));
        vp.fit_bounds(
            WorldBounds {
                min: Vec2::new(-10.0, -10.0),
                max: Vec2::new(110.0, 10.0),
            },
            0.0,
        );
        (g, positions, vp, GraphStyle::default())
    }

    #[test]
    fn hits_node_at_center() {
        let (g, pos, vp, style) = setup();
        let screen = vp.world_to_screen(Vec2::new(0.0, 0.0));
        let result = hit_test(&g, &pos, &vp, &style, screen);
        assert!(result.node.is_some());
        assert!(result.edge.is_none());
    }

    #[test]
    fn hits_edge_midpoint() {
        let (g, pos, vp, style) = setup();
        let screen = vp.world_to_screen(Vec2::new(50.0, 0.0));
        let result = hit_test(&g, &pos, &vp, &style, screen);
        assert!(result.edge.is_some());
        assert!(result.node.is_none());
    }

    #[test]
    fn misses_when_far_away() {
        let (g, pos, vp, style) = setup();
        let screen = vp.world_to_screen(Vec2::new(50.0, 500.0));
        let result = hit_test(&g, &pos, &vp, &style, screen);
        assert!(!result.is_hit());
    }

    #[test]
    fn node_takes_precedence_over_edge() {
        let (g, pos, vp, style) = setup();
        // Point at node a's center, which is also on the edge.
        let screen = vp.world_to_screen(Vec2::new(0.0, 0.0));
        let result = hit_test(&g, &pos, &vp, &style, screen);
        assert!(result.node.is_some());
        assert!(result.edge.is_none());
    }
}
