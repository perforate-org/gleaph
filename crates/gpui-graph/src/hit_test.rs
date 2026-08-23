//! Hit testing (§21).
//!
//! Hit testing uses two conceptual stages: a coarse spatial-index candidate
//! lookup (§20) followed by a precise geometry test. The synchronized runtime's
//! uniform-grid index narrows the candidate set to the primitives near the
//! pointer, then the precise node and edge tests run only on those candidates.

use glam::Vec2;
use std::hash::BuildHasher;

use crate::graph::{EdgeId, NodeId};
use crate::runtime::{EdgePrep, SyncedGraphRuntime};
use crate::style::GraphStyle;
use crate::viewport::{Viewport, WorldBounds};

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

/// Hit-test a screen point against the graph using the scene's spatial index.
///
/// Instead of scanning every node and edge, it queries the synchronized
/// runtime's uniform-grid spatial index for candidate nodes and edge bounding
/// boxes near the screen point, then runs a precise geometry test only on those
/// candidates. Node and edge identity, geometry, and the edge curve context all
/// come from the one immutable scene snapshot that the runtime proof borrows, so
/// the selectable geometry matches the drawn geometry exactly.
pub fn hit_test<NK, EK, N, E, S>(
    synced: &SyncedGraphRuntime<'_, NK, EK, N, E, S>,
    viewport: &Viewport,
    style: &GraphStyle,
    screen_point: Vec2,
) -> HitTestResult
where
    NK: Eq + std::hash::Hash,
    EK: Eq + std::hash::Hash,
    S: BuildHasher + Default + Clone,
{
    let scene = synced.scene;
    let graph = scene.graph();
    let node_position = &|id: NodeId| scene.node_position(id);
    let node_cluster_center = &|id: NodeId| scene.node_cluster_center(id);

    // The point's world position; candidate lookup works in world space, then
    // the precise geometry test is run in screen space against the transformed
    // candidates.
    let world_point = viewport.screen_to_world(screen_point);
    // Candidate margin so an edge whose curve bows near the point is not missed
    // by the coarse grid; the precise test still filters exactly.
    let margin = (style.node_radius * 2.0).max(style.edge_width + 2.0);
    let bounds = WorldBounds {
        min: world_point - Vec2::splat(margin),
        max: world_point + Vec2::splat(margin),
    };

    // Precise node test over indexed candidates.
    let mut best_node: Option<(NodeId, f32)> = None;
    for id in synced.visible_nodes(&bounds, margin) {
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

    // Precise edge test over indexed candidates. Reuse the zoom-invariant edge
    // preparation from the runtime so density, parallel groups, midpoints, and
    // normals are not rebuilt per mouse event.
    let prep: &EdgePrep<S> = synced.edges();
    let candidates = synced.visible_edge_candidates(&bounds, margin);

    // Cheap conservative reject before any curve work: drop candidates whose
    // indexed bounding box cannot come near the pointer. The box is the same
    // extent the spatial index trusts, so the surviving set is unchanged;
    // self-loops and oversized boxes carry an unbounded extent and always
    // survive to the precise test. The screen-space hit threshold widens the
    // box so wide-zoom pointers still see every near-miss curve.
    let edge_threshold = (style.edge_width * 0.5 + 2.0).max(3.0);
    let expand = margin.max(edge_threshold / viewport.zoom().max(f32::EPSILON));
    let candidates: Vec<usize> = candidates
        .into_iter()
        .filter(|&index| {
            let (lo, hi) = &prep.curve_bboxes[index];
            world_point.x >= lo.x - expand
                && world_point.x <= hi.x + expand
                && world_point.y >= lo.y - expand
                && world_point.y <= hi.y + expand
        })
        .collect();

    // The precise curve path for the candidate edges needs the same context the
    // paint layer builds. Obstacles come from the scene's node positions.
    // Filter explicitly by the same predicate `endpoints_in_field` uses
    // below: `visible_nodes` may fall back to returning every node for a
    // degenerate query, and the field must contain exactly what membership
    // claims. Coordinates stay world until after the filter.
    let obstacles_screen: Vec<Vec2> = synced
        .visible_nodes(&bounds, margin)
        .iter()
        .filter_map(|&id| node_position(id))
        .filter(|world| crate::paint::point_in_bounds(*world, &bounds, margin))
        .map(|world| viewport.world_to_screen(world))
        .collect();
    let obstacle_radius = style.node_radius * 2.0 + crate::paint::OBSTACLE_RADIUS;
    let obstacles_field = crate::paint::ObstacleField::new(&obstacles_screen, obstacle_radius);

    // Compute signed density only for the candidate edges (the grid is already
    // built over every edge's midpoint by the runtime).
    let signed_densities = crate::paint::signed_densities_for(
        &prep.density_grid,
        &prep.midpoints,
        &prep.normals,
        crate::paint::DENSITY_RADIUS,
        &candidates,
    );

    let mut best_edge: Option<(EdgeId, f32)> = None;
    for &index in &candidates {
        let id = prep.edge_ids[index];
        let edge = graph.edge(id).expect("edge exists");
        let path = crate::paint::edge_path(
            edge,
            &crate::paint::EdgeCurveContext {
                index,
                signed_density: signed_densities[index],
                has_reverse: &prep.has_reverse,
                parallel: &prep.parallel,
                obstacles: &obstacles_field,
                node_radius: style.node_radius,
                endpoints_in_field: (
                    crate::paint::point_in_bounds(prep.source[index], &bounds, margin),
                    crate::paint::point_in_bounds(prep.target[index], &bounds, margin),
                ),
            },
            graph,
            node_position,
            node_cluster_center,
            viewport,
            style,
        );
        let dist = path
            .iter()
            .map(|(p0, p1, p2)| distance_to_quadratic_bezier(screen_point, *p0, *p1, *p2))
            .fold(f32::INFINITY, f32::min);
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

/// Distance from a point to a quadratic Bézier curve, sampled at fixed steps.
fn distance_to_quadratic_bezier(p: Vec2, a: Vec2, control: Vec2, b: Vec2) -> f32 {
    const SAMPLES: usize = 16;
    let mut min = f32::INFINITY;
    let mut prev = a;
    for i in 1..=SAMPLES {
        let t = i as f32 / SAMPLES as f32;
        let inv = 1.0 - t;
        let point = inv * inv * a + 2.0 * inv * t * control + t * t * b;
        min = min.min(distance_to_segment(p, prev, point));
        prev = point;
    }
    min
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
    use crate::graph::EdgeDirection;
    use crate::patch::GraphBatch;
    use crate::scene::GraphScene;
    use crate::viewport::WorldBounds;

    /// Build a scene with nodes `a` (0,0) and `b` (100,0), a directed edge, and
    /// a self-loop on `a`, fitted to a viewport so everything is hittable.
    fn scene_setup() -> (GraphScene<&'static str, &'static str, (), ()>, Viewport) {
        let mut scene = GraphScene::new();
        scene.merge(
            GraphBatch::new()
                .node("a", ())
                .node("b", ())
                .edge("ab", "a", "b", EdgeDirection::Directed, ())
                .edge("loop", "a", "a", EdgeDirection::Directed, ()),
        );
        let a = scene.node_id(&"a").unwrap();
        let b = scene.node_id(&"b").unwrap();
        scene.set_position(a, Vec2::new(0.0, 0.0));
        scene.set_position(b, Vec2::new(100.0, 0.0));
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(200.0, 200.0));
        vp.fit_bounds(
            WorldBounds {
                min: Vec2::new(-10.0, -10.0),
                max: Vec2::new(110.0, 10.0),
            },
            0.0,
        );
        (scene, vp)
    }

    /// Run the indexed hit test at a world point, building the runtime from the
    /// scene and translating the point to screen space.
    fn hit_at(
        scene: &GraphScene<&'static str, &'static str, (), ()>,
        vp: &Viewport,
        world: Vec2,
    ) -> HitTestResult {
        let mut runtime = crate::runtime::GraphRuntime::new();
        let synced = scene.sync_runtime(&mut runtime);
        hit_test(
            &synced,
            vp,
            &GraphStyle::default(),
            vp.world_to_screen(world),
        )
    }

    #[test]
    fn hits_node_at_center() {
        let (scene, vp) = scene_setup();
        let result = hit_at(&scene, &vp, Vec2::new(0.0, 0.0));
        assert!(result.node.is_some());
        assert!(result.edge.is_none());
    }

    #[test]
    fn hits_edge_midpoint() {
        let (scene, vp) = scene_setup();
        // A lone edge with no neighbors is straight, so its midpoint is at
        // (50, 0).
        let result = hit_at(&scene, &vp, Vec2::new(50.0, 0.0));
        assert!(result.edge.is_some());
        assert!(result.node.is_none());
    }

    #[test]
    fn misses_when_far_away() {
        let (scene, vp) = scene_setup();
        let result = hit_at(&scene, &vp, Vec2::new(50.0, 500.0));
        assert!(!result.is_hit());
    }

    #[test]
    fn node_takes_precedence_over_edge() {
        let (scene, vp) = scene_setup();
        // Point at node a's center, which is also on the edge.
        let result = hit_at(&scene, &vp, Vec2::new(0.0, 0.0));
        assert!(result.node.is_some());
        assert!(result.edge.is_none());
    }

    #[test]
    fn hits_curved_parallel_edge() {
        let mut scene = GraphScene::new();
        scene.merge(
            GraphBatch::new()
                .node("a", ())
                .node("b", ())
                .edge("e0", "a", "b", EdgeDirection::Directed, ())
                .edge("e1", "a", "b", EdgeDirection::Directed, ()),
        );
        let a = scene.node_id(&"a").unwrap();
        let b = scene.node_id(&"b").unwrap();
        scene.set_position(a, Vec2::new(0.0, 0.0));
        scene.set_position(b, Vec2::new(100.0, 0.0));
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(200.0, 200.0));
        vp.fit_bounds(
            WorldBounds {
                min: Vec2::new(-10.0, -10.0),
                max: Vec2::new(110.0, 10.0),
            },
            0.0,
        );
        // With two parallel edges between the same node pair their midpoints
        // coincide, so the perpendicular density is zero and only the fan offset
        // applies; the trimmed curve's midpoint sits near world (50, 14.4).
        let result = hit_at(&scene, &vp, Vec2::new(50.0, 14.4));
        assert!(
            result.edge.is_some(),
            "curved parallel edge should be hittable"
        );
    }

    #[test]
    fn hits_self_loop_onigiri() {
        // A self-loop-only scene: the onigiri base points straight up (no other
        // incident edges to bias its direction), so it sits above the node.
        let mut scene = GraphScene::new();
        scene.merge(GraphBatch::new().node("a", ()).edge(
            "loop",
            "a",
            "a",
            EdgeDirection::Directed,
            (),
        ));
        let a = scene.node_id(&"a").unwrap();
        scene.set_position(a, Vec2::new(0.0, 0.0));
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(200.0, 200.0));
        vp.fit_bounds(
            WorldBounds {
                min: Vec2::new(-50.0, -100.0),
                max: Vec2::new(50.0, 10.0),
            },
            0.0,
        );
        // The onigiri base sits 8.5px above the node center, scaled by the
        // zoom. Hit at the base center.
        let scale = vp.zoom();
        let node_screen = vp.world_to_screen(Vec2::new(0.0, 0.0));
        let screen = node_screen + Vec2::new(0.0, -8.5 * scale);
        let mut runtime = crate::runtime::GraphRuntime::new();
        let synced = scene.sync_runtime(&mut runtime);
        let result = hit_test(&synced, &vp, &GraphStyle::default(), screen);
        assert!(
            result.edge.is_some(),
            "self-loop onigiri should be hittable at its base"
        );
    }
}
