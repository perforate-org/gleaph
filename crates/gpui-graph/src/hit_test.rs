//! Hit testing (§21).
//!
//! Hit testing uses two conceptual stages: coarse candidate lookup followed by
//! a precise geometry test. v0.1 uses a direct scan for both stages; the exact
//! spatial index is deliberately not fixed and should be selected empirically
//! (§21, §37).

use glam::Vec2;
use std::hash::BuildHasher;

use crate::graph::{EdgeId, Graph, NodeId};
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

/// Hit-test a screen point against the graph's nodes and edges.
///
/// Nodes are tested by distance to their screen-space center within the node
/// radius. Edges are tested by distance from the point to the screen-space
/// segment. Nodes take precedence over edges when both are hit.
pub fn hit_test<N, E>(
    graph: &Graph<N, E>,
    node_position: &impl Fn(NodeId) -> Option<Vec2>,
    node_cluster_center: &impl Fn(NodeId) -> Option<(Vec2, f32)>,
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
    // Collect obstacle node screen positions so the selectable edge geometry
    // matches the drawn curves (which bow around nodes). The grid lets each
    // edge test only the nodes near its chord instead of every node.
    let obstacles: Vec<Vec2> = graph
        .nodes()
        .filter_map(|(id, _)| node_position(id))
        .map(|world| viewport.world_to_screen(world))
        .collect();
    let obstacle_cell = style.node_radius * 2.0 + crate::paint::OBSTACLE_RADIUS;
    let obstacles_grid: crate::paint::ObstacleGrid<std::collections::hash_map::RandomState> =
        crate::paint::ObstacleGrid::new(&obstacles, obstacle_cell);
    // Group edges by their (source, target) node pair to detect parallels, so
    // curve control points match the paint layer.
    let mut groups: std::collections::HashMap<(NodeId, NodeId), Vec<usize>> =
        std::collections::HashMap::new();
    let mut edge_ids: Vec<EdgeId> = Vec::new();
    let mut midpoints: Vec<Vec2> = Vec::new();
    let mut normals: Vec<Vec2> = Vec::new();
    for (id, edge) in graph.edges() {
        let Some(source_world) = node_position(edge.source) else {
            continue;
        };
        let Some(target_world) = node_position(edge.target) else {
            continue;
        };
        let index = edge_ids.len();
        groups
            .entry((edge.source, edge.target))
            .or_default()
            .push(index);
        edge_ids.push(id);
        midpoints.push((source_world + target_world) * 0.5);
        let dir = target_world - source_world;
        normals.push(
            crate::paint::finite_chord_length(source_world, target_world)
                .map(|len| Vec2::new(-dir.y, dir.x) / len)
                .unwrap_or(Vec2::new(0.0, -1.0)),
        );
    }
    let signed_densities =
        crate::paint::signed_densities(&midpoints, &normals, crate::paint::DENSITY_RADIUS);
    let has_reverse: Vec<bool> = edge_ids
        .iter()
        .map(|id| {
            let edge = graph.edge(*id).expect("edge exists");
            groups.contains_key(&(edge.target, edge.source))
        })
        .collect();
    let parallel: Vec<Option<(usize, usize)>> = edge_ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let edge = graph.edge(*id).expect("edge exists");
            let group = &groups[&(edge.source, edge.target)];
            if group.len() > 1 {
                let position = group.iter().position(|&i| i == index).unwrap_or(0);
                Some((position, group.len()))
            } else {
                None
            }
        })
        .collect();
    for (index, id) in edge_ids.iter().enumerate() {
        let edge = graph.edge(*id).expect("edge exists");
        // Build the same trimmed path as the paint layer so the selectable
        // geometry matches what is drawn.
        let path = crate::paint::edge_path(
            edge,
            &crate::paint::EdgeCurveContext {
                index,
                signed_density: signed_densities[index],
                has_reverse: &has_reverse,
                parallel: &parallel,
                obstacles: &obstacles_grid,
                node_radius: style.node_radius,
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
            best_edge = Some((*id, dist));
        }
    }

    HitTestResult {
        node: None,
        edge: best_edge.map(|(e, _)| e),
    }
}

/// Hit-test a screen point against the graph using the scene's spatial index.
///
/// This is the indexed counterpart of [`hit_test`]: instead of scanning every
/// node and edge, it queries the synchronized runtime's uniform-grid spatial
/// index for candidate nodes and edge bounding boxes near the screen point, then
/// runs the same precise geometry test only on those candidates. Node and edge
/// identity, geometry, and the edge curve context all come from the one
/// immutable scene snapshot that the runtime proof borrows, so the selectable
/// geometry matches the drawn geometry exactly.
pub fn hit_test_indexed<NK, EK, N, E, S>(
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

    // The precise curve path for the candidate edges needs the same context the
    // paint layer builds. Obstacles come from the scene's node positions.
    let obstacles_screen: Vec<Vec2> = synced
        .visible_nodes(&bounds, margin)
        .iter()
        .filter_map(|&id| node_position(id))
        .map(|world| viewport.world_to_screen(world))
        .collect();
    let obstacle_cell = style.node_radius * 2.0 + crate::paint::OBSTACLE_RADIUS;
    let obstacles_grid: crate::paint::ObstacleGrid<S> =
        crate::paint::ObstacleGrid::new_with_hasher(&obstacles_screen, obstacle_cell, S::default());

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
                obstacles: &obstacles_grid,
                node_radius: style.node_radius,
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
    use crate::graph::{EdgeDirection, Graph};
    use crate::viewport::WorldBounds;

    fn no_clusters() -> impl Fn(NodeId) -> Option<(Vec2, f32)> {
        |_id| None
    }

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
        let result = hit_test(&g, &pos, &no_clusters(), &vp, &style, screen);
        assert!(result.node.is_some());
        assert!(result.edge.is_none());
    }

    #[test]
    fn hits_edge_midpoint() {
        let (g, pos, vp, style) = setup();
        // A lone edge with no neighbors is straight, so its midpoint is at
        // (50, 0).
        let screen = vp.world_to_screen(Vec2::new(50.0, 0.0));
        let result = hit_test(&g, &pos, &no_clusters(), &vp, &style, screen);
        assert!(result.edge.is_some());
        assert!(result.node.is_none());
    }

    #[test]
    fn misses_when_far_away() {
        let (g, pos, vp, style) = setup();
        let screen = vp.world_to_screen(Vec2::new(50.0, 500.0));
        let result = hit_test(&g, &pos, &no_clusters(), &vp, &style, screen);
        assert!(!result.is_hit());
    }

    #[test]
    fn node_takes_precedence_over_edge() {
        let (g, pos, vp, style) = setup();
        // Point at node a's center, which is also on the edge.
        let screen = vp.world_to_screen(Vec2::new(0.0, 0.0));
        let result = hit_test(&g, &pos, &no_clusters(), &vp, &style, screen);
        assert!(result.node.is_some());
        assert!(result.edge.is_none());
    }

    #[test]
    fn hits_curved_parallel_edge() {
        let mut g = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Directed, ());
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
        let style = GraphStyle::default();
        // The fanned curve bows toward its control point. With two parallel
        // edges between the same node pair, their midpoints coincide, so the
        // perpendicular density is zero and only the fan offset applies. The
        // With world-normalized fan spacing, the trimmed curve's midpoint sits
        // near world (50, 14.4); hit there.
        let screen = vp.world_to_screen(Vec2::new(50.0, 14.4));
        let result = hit_test(&g, &positions, &no_clusters(), &vp, &style, screen);
        assert!(
            result.edge.is_some(),
            "curved parallel edge should be hittable"
        );
    }

    #[test]
    fn hits_self_loop_onigiri() {
        let mut g = Graph::new();
        let a = g.add_node(());
        g.add_edge(a, a, EdgeDirection::Directed, ());
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else {
                None
            }
        };
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(200.0, 200.0));
        vp.fit_bounds(
            WorldBounds {
                min: Vec2::new(-50.0, -100.0),
                max: Vec2::new(50.0, 10.0),
            },
            0.0,
        );
        let style = GraphStyle::default();
        // The onigiri base sits 8.5px above the node center, scaled by the
        // zoom. Hit at the base center.
        let scale = vp.zoom();
        let node_screen = vp.world_to_screen(Vec2::new(0.0, 0.0));
        let screen = node_screen + Vec2::new(0.0, -8.5 * scale);
        let result = hit_test(&g, &positions, &no_clusters(), &vp, &style, screen);
        assert!(
            result.edge.is_some(),
            "self-loop onigiri should be hittable at its base"
        );
    }

    /// Build a scene with a node, a directed edge, and a self-loop, placed and
    /// fitted so both are hittable, for parity between the linear `hit_test`
    /// and the indexed `hit_test_indexed`.
    fn scene_setup() -> (
        crate::scene::GraphScene<&'static str, &'static str, (), ()>,
        Viewport,
    ) {
        let mut scene = crate::scene::GraphScene::new();
        scene.merge(
            crate::patch::GraphBatch::new()
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

    /// The indexed hit test must agree with the linear scan on the same scene,
    /// because the two code paths must not diverge as the graph grows.
    #[test]
    fn indexed_hit_matches_linear() {
        let (scene, vp) = scene_setup();
        let mut runtime = crate::runtime::GraphRuntime::new();
        let style = GraphStyle::default();
        // Probe a spread of screen points spanning the node, the edge midpoint,
        // the self-loop base, and empty space. Each must agree between the two
        // paths.
        for (world, label) in [
            (Vec2::new(0.0, 0.0), "node a"),
            (Vec2::new(50.0, 0.0), "edge midpoint"),
            (Vec2::new(0.0, -6.0), "near node a (self-loop base)"),
            (Vec2::new(50.0, 500.0), "far away"),
        ] {
            let screen = vp.world_to_screen(world);
            let linear = {
                let synced = scene.sync_runtime(&mut runtime);
                // Re-read graph/positions from the synced snapshot for the
                // linear path too, so both operate on the same state.
                let g = synced.scene.graph();
                let pos = &|id: NodeId| synced.scene.node_position(id);
                let clusters = &|id: NodeId| synced.scene.node_cluster_center(id);
                hit_test(g, pos, clusters, &vp, &style, screen)
            };
            let indexed = {
                let synced = scene.sync_runtime(&mut runtime);
                hit_test_indexed(&synced, &vp, &style, screen)
            };
            assert_eq!(
                indexed.node, linear.node,
                "node parity mismatch at {label} (world {world:?})"
            );
            assert_eq!(
                indexed.edge, linear.edge,
                "edge parity mismatch at {label} (world {world:?})"
            );
        }
    }
}
