//! Paint frame (§18.2).
//!
//! A [`PaintFrame`] is an intermediate frame representation containing only the
//! information required for the current paint: visible nodes and edges already
//! transformed to canvas-local pixels, plus interaction highlights. This
//! separates graph and scene state from rendering mechanics. The `GraphView`
//! boundary owns the separate conversion between canvas-local and window-space
//! GPUI coordinates.

use glam::Vec2;

use crate::graph::{Edge, EdgeDirection, EdgeId, Graph, NodeId};
use crate::interaction::{Hover, Selection};
use crate::style::GraphStyle;
use crate::viewport::Viewport;

/// A quadratic Bézier curve `(p0, p1, p2)`.
pub type Bezier = (Vec2, Vec2, Vec2);

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
#[derive(Debug, Clone)]
pub struct PaintEdge {
    /// Stable edge identity.
    pub id: EdgeId,
    /// Canvas-local pixel source position (the source node center).
    pub source: Vec2,
    /// Canvas-local pixel target position (the target node center).
    pub target: Vec2,
    /// The trimmed quadratic Bézier path to draw. A self-loop is a list of
    /// onigiri segments; any other edge is a single segment trimmed to the node
    /// boundaries. The path already stops just outside each node boundary.
    pub path: Vec<Bezier>,
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
    /// Unit offset direction to shift the label off the edge line.
    pub offset: Vec2,
    /// The label text.
    pub text: String,
    /// The edge's trimmed path, in canvas-local pixels. Empty for a self-loop,
    /// whose label stays fixed at `position`.
    pub path: Vec<Bezier>,
    /// Position along `path` in `[0, 1]` where the label sits. Collision
    /// resolution slides this to move labels apart smoothly along their edges.
    pub t: f32,
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
    /// Resolves a node's cluster center and radius, if the layout grouped it
    /// into a cluster. Used to bow edges within a cluster outward from its
    /// center.
    pub node_cluster_center: &'a dyn Fn(NodeId) -> Option<(Vec2, f32)>,
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
        node_cluster_center,
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

    // Collect obstacle node screen positions so edges can bow around nodes they
    // would otherwise pass through. The grid lets each edge test only the nodes
    // near its chord instead of every node.
    let mut obstacles_screen: Vec<Vec2> = Vec::new();
    for (id, _) in graph.nodes() {
        let Some(world) = node_position(id) else {
            continue;
        };
        if world.x < visible.min.x - margin
            || world.x > visible.max.x + margin
            || world.y < visible.min.y - margin
            || world.y > visible.max.y + margin
        {
            continue;
        }
        obstacles_screen.push(viewport.world_to_screen(world));
    }
    let obstacle_cell = style.node_radius * 2.0 + OBSTACLE_RADIUS;
    let obstacles_screen_grid = ObstacleGrid::new(&obstacles_screen, obstacle_cell);
    // An empty grid used only for culling: the cull test only needs the curve's
    // bounding box, so it skips node avoidance (which is applied later, when
    // the edge is actually drawn). This keeps off-screen edges cheap.
    let empty_obstacle_grid = ObstacleGrid::new(&[], obstacle_cell);

    // Collect candidate edges, then assign curve control points so parallel
    // edges and self-loops are separated visually.
    let mut candidate_edges: Vec<(EdgeId, &Edge<E>, Vec2, Vec2)> = Vec::new();
    for (id, edge) in graph.edges() {
        let Some(source_world) = node_position(edge.source) else {
            continue;
        };
        let Some(target_world) = node_position(edge.target) else {
            continue;
        };
        candidate_edges.push((id, edge, source_world, target_world));
    }

    // Group edges by their (source, target) node pair to detect parallels.
    let mut groups: std::collections::HashMap<(NodeId, NodeId), Vec<usize>> =
        std::collections::HashMap::new();
    for (index, (_, edge, _, _)) in candidate_edges.iter().enumerate() {
        groups
            .entry((edge.source, edge.target))
            .or_default()
            .push(index);
    }
    // Precompute whether each edge has a reverse edge (target -> source), so
    // cluster edges can separate the two directions of a 2-node SCC in O(1)
    // instead of scanning every group.
    let has_reverse: Vec<bool> = candidate_edges
        .iter()
        .map(|(_, edge, _, _)| groups.contains_key(&(edge.target, edge.source)))
        .collect();
    // Precompute each edge's position within its parallel group and the group's
    // size, so the parallel fan is O(1) instead of scanning every group.
    let parallel: Vec<Option<(usize, usize)>> = candidate_edges
        .iter()
        .enumerate()
        .map(|(index, (_, edge, _, _))| {
            let group = &groups[&(edge.source, edge.target)];
            if group.len() > 1 {
                let position = group.iter().position(|&i| i == index).unwrap_or(0);
                Some((position, group.len()))
            } else {
                None
            }
        })
        .collect();

    // Compute each edge's world-space midpoint and normal, then the signed local
    // edge density (neighbors on the left minus on the right). Density is
    // computed in world space so the neighbor set is zoom-invariant.
    let midpoints: Vec<Vec2> = candidate_edges
        .iter()
        .map(|(_, _, s, t)| (*s + *t) * 0.5)
        .collect();
    let normals: Vec<Vec2> = candidate_edges
        .iter()
        .map(|(_, _, s, t)| {
            let dir = *t - *s;
            let len = dir.length();
            if len < f32::EPSILON {
                Vec2::new(0.0, -1.0)
            } else {
                Vec2::new(-dir.y, dir.x) / len
            }
        })
        .collect();
    let signed_densities = signed_densities(&midpoints, &normals, DENSITY_RADIUS);

    let mut visible_edges: Vec<(usize, EdgeId, &Edge<E>, Vec2, Vec2)> = Vec::new();
    for (index, (id, edge, source_world, target_world)) in candidate_edges.iter().enumerate() {
        // Cull edges whose curve's bounding box is entirely outside the visible
        // bounds. A curved edge may pass through the view even when both
        // endpoints are outside it, so the control point is included in the
        // bounds test.
        let source_visible = point_in_bounds(*source_world, &visible, margin);
        let target_visible = point_in_bounds(*target_world, &visible, margin);
        if !source_visible && !target_visible {
            // Both endpoints are outside; keep the edge only if its curve
            // (including the control point) still crosses the visible bounds.
            let is_self_loop = (*source_world - *target_world).length() < f32::EPSILON;
            let curve_visible = if is_self_loop {
                // A self-loop's onigiri path may extend well beyond the node,
                // so test the path's bounding box.
                let path = self_loop_path(
                    edge.source,
                    *source_world,
                    graph,
                    node_position,
                    viewport,
                    style,
                );
                let mut min = Vec2::splat(f32::INFINITY);
                let mut max = Vec2::splat(f32::NEG_INFINITY);
                for (p0, p1, p2) in &path {
                    min = min.min(*p0).min(*p1).min(*p2);
                    max = max.max(*p0).max(*p1).max(*p2);
                }
                bounds_intersect(&visible, margin, min, max)
            } else {
                let cluster_center =
                    shared_cluster_center(edge.source, edge.target, node_cluster_center);
                let cull_ctx = EdgeCurveContext {
                    index,
                    signed_density: signed_densities[index],
                    has_reverse: &has_reverse,
                    parallel: &parallel,
                    zoom: viewport.zoom(),
                    obstacles: &empty_obstacle_grid,
                    node_radius: style.node_radius,
                };
                let control_world =
                    edge_control_point(*source_world, *target_world, &cull_ctx, cluster_center);
                let min = (*source_world).min(*target_world).min(control_world);
                let max = (*source_world).max(*target_world).max(control_world);
                bounds_intersect(&visible, margin, min, max)
            };
            if !curve_visible {
                continue;
            }
        }
        visible_edges.push((
            index,
            *id,
            edge,
            viewport.world_to_screen(*source_world),
            viewport.world_to_screen(*target_world),
        ));
    }

    for (candidate_index, id, edge, source, target) in visible_edges.iter() {
        let is_self_loop = (*source - *target).length() < f32::EPSILON;
        let path = edge_path(
            edge,
            &EdgeCurveContext {
                index: *candidate_index,
                signed_density: signed_densities[*candidate_index],
                has_reverse: &has_reverse,
                parallel: &parallel,
                zoom: viewport.zoom(),
                obstacles: &obstacles_screen_grid,
                node_radius: style.node_radius,
            },
            graph,
            node_position,
            node_cluster_center,
            viewport,
            style,
        );
        // Skip degenerate edges (e.g. overlapping nodes) whose trimmed path is
        // empty, so no non-finite geometry reaches the paint layer.
        if path.is_empty() {
            continue;
        }
        let apex = if is_self_loop {
            path.first().map(|(_, _, p2)| *p2)
        } else {
            None
        };
        // Compute the label position before moving `path` into the edge.
        let label = edge_label(*id, &edge.data).map(|text| {
            if is_self_loop {
                // A self-loop's label sits at the onigiri's base center (away
                // from the node) so it is clear of the node. It carries the
                // loop's path so it can slide along it to avoid collisions.
                (
                    apex.expect("self-loop has a base"),
                    Vec2::new(0.0, -1.0),
                    text,
                    path.clone(),
                    0.5,
                )
            } else {
                // The label sits at the midpoint of the trimmed curve, so
                // parallel edges (which bow to different control points) get
                // distinct label positions instead of overlapping.
                let (p0, p1, p2) = path[0];
                let position = 0.25 * p0 + 0.5 * p1 + 0.25 * p2;
                let tangent = *target - *source;
                let len = tangent.length();
                // Normalize the normal so its y component is always upward.
                // This keeps labels on the same side of the edge regardless of
                // whether the edge points left or right.
                let normal = if len > f32::EPSILON {
                    let n = Vec2::new(-tangent.y, tangent.x) / len;
                    if n.y < 0.0 { -n } else { n }
                } else {
                    Vec2::new(0.0, -1.0)
                };
                (position, normal, text, path.clone(), 0.5)
            }
        });
        frame.edges.push(PaintEdge {
            id: *id,
            source: *source,
            target: *target,
            path,
            direction: edge.direction,
            selected: selection.contains_edge(*id),
            hovered: hover.edge == Some(*id),
        });
        if let Some((position, offset, text, path, t)) = label {
            frame.edge_labels.push(PaintEdgeLabel {
                position,
                offset,
                text,
                path,
                t,
            });
        }
    }

    frame
}

/// Radius (in world units) within which other edges count toward an edge's
/// local density. Computed in world space so the neighbor set is zoom-invariant.
pub(crate) const DENSITY_RADIUS: f32 = 40.0;
/// Base world-space spacing between parallel edges, at the reference apparent
/// length. The actual spacing scales with the edge's apparent length (world
/// length times zoom): a shorter apparent distance yields a narrower spacing, a
/// longer apparent distance a wider spacing, so parallel edges keep a consistent
/// on-screen separation. The power is sub-linear so the sagitta grows more
/// slowly than the chord and curvature still drops as the node distance grows.
const PARALLEL_SPACING: f32 = 60.0;
/// Reference apparent length (world length * zoom) at which the spacing is the
/// base value.
const PARALLEL_SPACING_REF_APPARENT: f32 = 100.0;
/// Power relating apparent length to spacing. 0 keeps spacing constant; 1 makes
/// spacing proportional to apparent length.
const PARALLEL_SPACING_POWER: f32 = 0.10;
/// Bow per unit of signed density difference, as a fraction of edge length.
const BOW_DENSITY: f32 = 0.20;
/// Upper bound on the bow as a fraction of edge length.
const BOW_MAX: f32 = 0.90;
/// Extra clearance around an obstacle node, in world units, that an edge's
/// control point is pushed away from.
pub(crate) const OBSTACLE_RADIUS: f32 = 30.0;
/// Multiplier applied to a cluster edge's chord distance to push the control
/// point outside the circle for outer edges, so they follow the circular arc.
/// For adjacent nodes (chord at `radius·cos(45°)`) a gain of ~1.0 places the
/// control point at the exact circular-arc position `radius/cos(45°)`.
const CLUSTER_GAIN: f32 = 0.9;
/// Base outward bow of a cluster edge, as a fraction of the cluster radius,
/// applied even to a chord through the center (a diameter) so it is not
/// perfectly straight.
const CLUSTER_BASE: f32 = 0.05;
/// Perpendicular offset of a cluster edge's control point along its left
/// normal, as a fraction of the cluster radius. Applied uniformly (like the
/// parallel fan) so the two directions of a 2-node SCC separate and stay
/// separated as nodes move, with no angle threshold.
const CLUSTER_NORMAL_OFFSET: f32 = 0.3;

/// A uniform grid over edge midpoints used to count nearby edges in O(E).
struct DensityGrid {
    cell_size: f32,
    cells: std::collections::HashMap<(i32, i32), Vec<usize>>,
}

impl DensityGrid {
    fn new(midpoints: &[Vec2], radius: f32) -> Self {
        let cell_size = radius;
        let mut cells: std::collections::HashMap<(i32, i32), Vec<usize>> =
            std::collections::HashMap::new();
        for (i, m) in midpoints.iter().enumerate() {
            let cell = (
                (m.x / cell_size).floor() as i32,
                (m.y / cell_size).floor() as i32,
            );
            cells.entry(cell).or_default().push(i);
        }
        Self { cell_size, cells }
    }

    /// Indices of edges whose midpoint may lie within `radius` of `midpoint`.
    fn candidates(&self, midpoint: Vec2, radius: f32) -> impl Iterator<Item = usize> + '_ {
        let cell = (
            (midpoint.x / self.cell_size).floor() as i32,
            (midpoint.y / self.cell_size).floor() as i32,
        );
        let span = (radius / self.cell_size).ceil() as i32;
        (-span..=span).flat_map(move |dx| {
            (-span..=span).flat_map(move |dy| {
                self.cells
                    .get(&(cell.0 + dx, cell.1 + dy))
                    .into_iter()
                    .flatten()
                    .copied()
            })
        })
    }
}

/// A uniform grid over obstacle node positions, so an edge only tests the
/// nodes near its chord instead of every node in the graph.
pub(crate) struct ObstacleGrid {
    cell_size: f32,
    cells: std::collections::HashMap<(i32, i32), Vec<Vec2>>,
}

impl ObstacleGrid {
    pub(crate) fn new(obstacles: &[Vec2], cell_size: f32) -> Self {
        let mut cells: std::collections::HashMap<(i32, i32), Vec<Vec2>> =
            std::collections::HashMap::new();
        for &o in obstacles {
            let cell = (
                (o.x / cell_size).floor() as i32,
                (o.y / cell_size).floor() as i32,
            );
            cells.entry(cell).or_default().push(o);
        }
        Self { cell_size, cells }
    }

    /// Obstacle positions that may lie within `radius` of `point`.
    fn candidates(&self, point: Vec2, radius: f32) -> impl Iterator<Item = Vec2> + '_ {
        let cell = (
            (point.x / self.cell_size).floor() as i32,
            (point.y / self.cell_size).floor() as i32,
        );
        let span = (radius / self.cell_size).ceil() as i32;
        (-span..=span).flat_map(move |dx| {
            (-span..=span).flat_map(move |dy| {
                self.cells
                    .get(&(cell.0 + dx, cell.1 + dy))
                    .into_iter()
                    .flatten()
                    .copied()
            })
        })
    }
}

/// Compute the signed local edge density for each edge: the distance-weighted
/// sum of other edges whose midpoint lies within `radius`, positive on the left
/// of this edge's direction and negative on the right. Each neighbor
/// contributes `cos(angle) * proximity`, where `cos(angle)` is the signed
/// perpendicular distance normalized by the neighbor's distance (so it is
/// continuous through zero as the neighbor crosses the edge's axis) and
/// `proximity` falls off linearly to zero at `radius`. This keeps the bow
/// continuous while a node is dragged (no hard sign flip, so edges do not
/// jitter) while still favoring closer neighbors. A positive value means more or
/// closer edges on the left, so the edge bows right (toward the lower-density
/// side).
pub(crate) fn signed_densities(midpoints: &[Vec2], normals: &[Vec2], radius: f32) -> Vec<f32> {
    let grid = DensityGrid::new(midpoints, radius);
    let mut result = vec![0.0f32; midpoints.len()];
    for i in 0..midpoints.len() {
        let mut signed = 0.0f32;
        for j in grid.candidates(midpoints[i], radius) {
            if i == j {
                continue;
            }
            let delta = midpoints[j] - midpoints[i];
            let dist = delta.length();
            if dist > radius || dist < f32::EPSILON {
                continue;
            }
            // Proximity weight: a neighbor at distance 0 contributes 1, at the
            // radius edge contributes 0.
            let proximity = 1.0 - dist / radius;
            // Signed perpendicular distance normalized by the neighbor's distance
            // (the cosine of the angle to the normal). This is continuous
            // through zero as the neighbor crosses the edge's axis, so the bow
            // does not jump.
            let cos_angle = normals[i].dot(delta) / dist;
            signed += cos_angle * proximity;
        }
        result[i] = signed;
    }
    result
}

/// Compute a quadratic Bézier control point for an edge.
///
/// Self-loops get a loop above the node. Parallel edges (multiple edges between
/// the same node pair) are fanned out perpendicular to the edge direction so
/// they do not overlap. Every non-loop edge bows toward the side with lower
/// local edge density. Every non-loop edge bows toward the side with lower
/// local edge density; a lone edge with no neighbors is straight.
///
/// When both endpoints share a cluster center (e.g. the center of an SCC
/// circle), the edge bows outward from that center so the cluster's circular
/// shape stays readable even when node spacing is large. The control point is
/// placed outside the cluster radius, so even a chord through the center bows
/// outward rather than inward. The cluster bow is a fixed fraction of the edge
/// length, independent of local density.
pub(crate) fn edge_control_point(
    source: Vec2,
    target: Vec2,
    ctx: &EdgeCurveContext<'_>,
    cluster: Option<(Vec2, f32)>,
) -> Vec2 {
    let dir = target - source;
    let len = dir.length();
    if len < f32::EPSILON {
        // Self-loop: control point above the node to create a loop.
        // The height is tuned to be approx 1.5x node radius.
        return source + Vec2::new(0.0, -80.0);
    }
    let unit = dir / len;
    let normal = Vec2::new(-unit.y, unit.x);
    let midpoint = (source + target) * 0.5;
    // Parallel fan: separate multiple edges between the same node pair.
    let mut offset = 0.0f32;
    if let Some((position, group_len)) = ctx.parallel[ctx.index] {
        // The spacing scales with the edge's apparent length (world length times
        // zoom): shorter apparent distance -> narrower spacing, longer -> wider.
        // The power is sub-linear so the sagitta grows more slowly than the
        // chord and curvature still drops for longer edges.
        let apparent = len * ctx.zoom;
        let spacing = PARALLEL_SPACING
            * (apparent / PARALLEL_SPACING_REF_APPARENT).powf(PARALLEL_SPACING_POWER);
        offset = (position as f32 - (group_len as f32 - 1.0) * 0.5) * spacing;
    }
    // Cluster bow: when both endpoints share a cluster center, bow the edge so
    // the cluster reads as a circle. The control point's distance from the
    // center is `chord_dist * (1 + gain) + base`, where `chord_dist` is the
    // distance from the center to the chord's midpoint. Outer edges (adjacent
    // nodes, chord near the circle) push the control point outside the circle
    // to follow the arc, while a chord through the center (a diameter) keeps it
    // near the center so the edge stays almost straight. A perpendicular offset
    // along the edge's left normal is always added, exactly like the parallel
    // fan, so the two directions of a 2-node SCC separate instead of
    // overlapping — and stay separated as nodes move, with no angle threshold.
    if let Some((center, radius)) = cluster {
        let v1 = source - center;
        let v2 = target - center;
        let d1 = v1.length().max(f32::EPSILON);
        let d2 = v2.length().max(f32::EPSILON);
        let cos_angle = (v1.dot(v2) / (d1 * d2)).clamp(-1.0, 1.0);
        // Distance from the center to the chord's midpoint: radius * cos(Δθ/2).
        let chord_dist = radius * ((1.0 + cos_angle) * 0.5).sqrt();
        // How close each node is to the circle. The bow fades as a node is
        // dragged off the circle, so edges to it do not become extreme or
        // U-turn when the node is far from the cluster.
        let adhere1 = (1.0 - (d1 - radius).abs() / radius).clamp(0.0, 1.0);
        let adhere2 = (1.0 - (d2 - radius).abs() / radius).clamp(0.0, 1.0);
        let adherence = adhere1.min(adhere2);
        // A perpendicular offset along the edge's left normal separates the two
        // directions of a 2-node SCC (e.g. 8<->9) so they do not overlap. It is
        // applied only when the reverse edge exists, so ordinary adjacent edges
        // (e.g. 6-7) are not pushed outward.
        let normal_offset = if ctx.has_reverse[ctx.index] {
            CLUSTER_NORMAL_OFFSET * radius
        } else {
            0.0
        };
        // Outward direction: the angle bisector of the two node directions
        // (v1/|v1| + v2/|v2|). This always points away from the center along
        // the arc. As the nodes approach a diameter the bisector shrinks; the
        // bow is faded continuously by `arc_weight` so the edge eases into a
        // straight line instead of snapping.
        let bisector = v1 / d1 + v2 / d2;
        let bisector_len = bisector.length();
        // arc_weight is 1 for adjacent nodes and eases to 0 as the nodes become
        // opposite (a diameter), where the arc is a straight line.
        let arc_weight = (bisector_len * 0.5).min(1.0);
        let outward = if bisector_len > f32::EPSILON {
            bisector / bisector_len
        } else {
            normal
        };
        // Control point at the chord midpoint plus a bow that fades with both
        // adherence (node dragged off the circle) and arc_weight (near-diameter),
        // so the transition to a straight edge is continuous.
        let bow = (chord_dist * CLUSTER_GAIN + CLUSTER_BASE * radius) * adherence * arc_weight;
        let control_dist = chord_dist + bow;
        let mut control = center + outward * control_dist + normal * normal_offset;
        apply_node_avoidance(&mut control, source, target, midpoint, unit, normal, ctx);
        return control;
    }
    // Density bow: bow toward the side with fewer neighbor edges. The bow is a
    // fraction of edge length, so the curve shape is stable under zoom. When the
    // signed density is zero (no neighbors, or balanced left/right), the bow is
    // zero and the edge is straight.
    let direction = if ctx.signed_density > 0.0 { -1.0 } else { 1.0 };
    let magnitude = (ctx.signed_density.abs() * BOW_DENSITY).min(BOW_MAX);
    let bow = direction * magnitude * len;
    let mut control = midpoint + normal * (offset + bow);
    apply_node_avoidance(&mut control, source, target, midpoint, unit, normal, ctx);
    control
}

/// Push `control` away from any obstacle node that lies near the chord, so the
/// edge does not run through another node.
///
/// For each obstacle, the signed perpendicular distance to the chord is
/// `(obstacle - midpoint) · normal`. If it is within the influence radius, the
/// control point is pushed perpendicular to the chord, away from the obstacle,
/// by the remaining clearance. This handles obstacles anywhere along the chord,
/// not just at its midpoint. The edge's own endpoints are skipped.
fn apply_node_avoidance(
    control: &mut Vec2,
    source: Vec2,
    target: Vec2,
    midpoint: Vec2,
    unit: Vec2,
    normal: Vec2,
    ctx: &EdgeCurveContext<'_>,
) {
    let half_len = (target - source).length() * 0.5;
    // Cap the total push so a short edge (e.g. at very low zoom, where edges
    // are only a few pixels on screen) does not bow far beyond its own length.
    // The quadratic Bézier's maximum offset is half the control point's
    // displacement, so capping the push at `half_len` keeps the curve within a
    // quarter of the edge length. At normal zoom the cap is large enough that
    // the full node clearance still applies.
    let max_push = half_len;
    let mut push = Vec2::ZERO;
    // Only test obstacles near the chord. The influence radius is the maximum
    // perpendicular distance at which an obstacle can still push the edge, and
    // an obstacle can sit anywhere along the segment (up to `half_len` from the
    // midpoint), so the grid query radius is the sum of the two.
    let influence_radius = ctx.node_radius * 2.0 + OBSTACLE_RADIUS;
    for obstacle in ctx
        .obstacles
        .candidates(midpoint, half_len + influence_radius)
    {
        // Skip the edge's own endpoints. Use a small tolerance so tiny
        // floating-point differences from the screen transform do not make the
        // edge treat its own endpoints as obstacles.
        if (obstacle - source).length_squared() < 1e-3
            || (obstacle - target).length_squared() < 1e-3
        {
            continue;
        }
        let to_obstacle = obstacle - midpoint;
        // Signed perpendicular distance from the obstacle to the chord line.
        let perp = to_obstacle.dot(normal);
        // Only consider obstacles whose projection onto the chord lies within
        // the edge's segment, so nodes beyond the endpoints do not influence
        // the edge. `along` is the signed distance from the midpoint along the
        // chord direction; the segment spans [-half_len, half_len].
        let along = to_obstacle.dot(unit);
        if along.abs() > half_len {
            continue;
        }
        let influence = (influence_radius - perp.abs()).max(0.0);
        if influence > 0.0 {
            // Push away from the obstacle: opposite the obstacle's side. The
            // quadratic Bézier's maximum offset is half the control point's
            // displacement, so double the push to clear the node.
            let away = if perp >= 0.0 { -normal } else { normal };
            push += away * influence * 2.0;
        }
    }
    if push.length() > max_push {
        push = push.normalize() * max_push;
    }
    *control += push;
}

/// Per-edge geometry context shared by the paint layer and hit testing so the
/// drawn and selectable curves always match.
pub(crate) struct EdgeCurveContext<'a> {
    /// This edge's index among all candidate edges.
    pub index: usize,
    /// Signed local edge density (neighbors on the left minus on the right).
    pub signed_density: f32,
    /// Whether each edge has a reverse edge (target -> source) in the same
    /// cluster, parallel to `groups`. Used to separate the two directions of a
    /// 2-node SCC without pushing ordinary adjacent edges outward.
    pub has_reverse: &'a [bool],
    /// For each edge, its position within its parallel group and the group's
    /// size, when the group has more than one edge; `None` for a lone edge.
    /// Parallel to `groups`, so the parallel fan is O(1) instead of scanning
    /// every group.
    pub parallel: &'a [Option<(usize, usize)>],
    /// Current zoom (pixels per world unit), used to vary parallel spacing.
    pub zoom: f32,
    /// A grid over obstacle node positions the edge should bow around, in the
    /// same coordinate space as the edge's endpoints.
    pub obstacles: &'a ObstacleGrid,
    /// Node radius, used to size the clearance around obstacle nodes.
    pub node_radius: f32,
}

/// The cluster center and radius shared by two nodes, if both belong to the
/// same cluster.
///
/// Returns `Some((center, radius))` only when both endpoints resolve to the
/// same cluster center, so edges within a cluster bow outward from it while
/// edges between clusters (which have different or no centers) keep their
/// normal behavior.
fn shared_cluster_center(
    source: NodeId,
    target: NodeId,
    node_cluster_center: &dyn Fn(NodeId) -> Option<(Vec2, f32)>,
) -> Option<(Vec2, f32)> {
    let (s, sr) = node_cluster_center(source)?;
    let (t, tr) = node_cluster_center(target)?;
    if (s - t).length_squared() < f32::EPSILON {
        Some((s, sr.max(tr)))
    } else {
        None
    }
}

/// Build the trimmed quadratic Bézier path for an edge, in screen/canvas-local
/// coordinates.
///
/// A self-loop returns the onigiri path; any other edge returns a single
/// segment trimmed to the node boundaries. Both the paint layer and hit testing
/// use this so the drawn and selectable geometry always match.
pub(crate) fn edge_path<N, E>(
    edge: &Edge<E>,
    ctx: &EdgeCurveContext<'_>,
    graph: &Graph<N, E>,
    node_position: &dyn Fn(NodeId) -> Option<Vec2>,
    node_cluster_center: &dyn Fn(NodeId) -> Option<(Vec2, f32)>,
    viewport: &Viewport,
    style: &GraphStyle,
) -> Vec<Bezier> {
    let source =
        viewport.world_to_screen(node_position(edge.source).expect("edge source has a position"));
    let target =
        viewport.world_to_screen(node_position(edge.target).expect("edge target has a position"));
    if (source - target).length() < f32::EPSILON {
        self_loop_path(edge.source, source, graph, node_position, viewport, style)
    } else {
        // The cluster center is in world coordinates; convert it to screen
        // space so the bow direction matches the screen-space edge geometry.
        let cluster_center = shared_cluster_center(edge.source, edge.target, node_cluster_center)
            .map(|(center, radius)| (viewport.world_to_screen(center), radius * viewport.zoom()));
        let control = edge_control_point(source, target, ctx, cluster_center);
        let curve = trim_curve_to_node_boundary(source, control, target, style.node_radius);
        // When the nodes overlap, the trimmed curve is degenerate (its start
        // parameter is not before its end), collapsing to a point. Return an
        // empty path so the edge is skipped rather than producing a zero-length
        // segment whose arrow would normalize to NaN.
        if (curve.2 - curve.0).length() > f32::EPSILON {
            vec![curve]
        } else {
            Vec::new()
        }
    }
}

/// Trim a quadratic Bézier edge `(source, control, target)` along its own path
/// so the endpoints stop just outside each node boundary, emerging from the
/// node center rather than the node edge.
///
/// `source` and `target` are the two node centers. The curve is trimmed at the
/// parameter `t` where it first leaves the source node's boundary and the
/// parameter where it last enters the target node's boundary, found by binary
/// search on the curve parameter so the result is smooth under zoom.
pub(crate) fn trim_curve_to_node_boundary(
    source: Vec2,
    control: Vec2,
    target: Vec2,
    radius: f32,
) -> Bezier {
    let gap = radius + 2.0;
    let t_start = boundary_t(source, control, target, source, gap, true);
    let t_end = boundary_t(source, control, target, target, gap, false);
    sub_bezier(source, control, target, t_start, t_end)
}

/// Find the parameter `t` where the curve first leaves (or last enters) the
/// node boundary centered at `center` with radius `gap`.
///
/// `leaving` is `true` to find the first `t` where the curve is at least `gap`
/// from `center` (the source end), and `false` to find the last `t` where it is
/// still at least `gap` from `center` (the target end). Binary search on the
/// curve parameter keeps the result smooth under zoom.
fn boundary_t(p0: Vec2, p1: Vec2, p2: Vec2, center: Vec2, gap: f32, leaving: bool) -> f32 {
    let mut lo = 0.0f32;
    let mut hi = 1.0f32;
    // 20 iterations give a parameter precision of ~1e-6, far below a pixel at
    // any zoom, so the result stays smooth while halving the per-edge cost.
    for _ in 0..20 {
        let mid = (lo + hi) * 0.5;
        let p = bezier_point(p0, p1, p2, mid);
        let outside = (p - center).length() >= gap;
        if leaving {
            // Move hi down while outside, lo up while inside.
            if outside {
                hi = mid;
            } else {
                lo = mid;
            }
        } else {
            // Move lo up while outside, hi down while inside.
            if outside {
                lo = mid;
            } else {
                hi = mid;
            }
        }
    }
    (lo + hi) * 0.5
}

/// A point on a quadratic Bézier at parameter `t`.
fn bezier_point(p0: Vec2, p1: Vec2, p2: Vec2, t: f32) -> Vec2 {
    let inv = 1.0 - t;
    inv * inv * p0 + 2.0 * inv * t * p1 + t * t * p2
}

/// The sub-curve of a quadratic Bézier from parameter `t0` to `t1`.
fn sub_bezier(p0: Vec2, p1: Vec2, p2: Vec2, t0: f32, t1: f32) -> Bezier {
    // Subdivide at t0 to get the [t0, 1] piece, then at the normalized t1.
    let (_, right) = subdivide(p0, p1, p2, t0);
    let s = (t1 - t0) / (1.0 - t0);
    let (left, _) = subdivide(right.0, right.1, right.2, s);
    left
}

/// Split a quadratic Bézier at parameter `t` into `[0, t]` and `[t, 1]`.
fn subdivide(p0: Vec2, p1: Vec2, p2: Vec2, t: f32) -> (Bezier, Bezier) {
    let ab = p0 + (p1 - p0) * t;
    let bc = p1 + (p2 - p1) * t;
    let abc = ab + (bc - ab) * t;
    ((p0, ab, abc), (abc, bc, p2))
}

/// Compute the onigiri self-loop path for a node.
///
/// The node is the apex (tip) of the onigiri; a wide, rounded base sits away
/// from the node. The path is a list of quadratic Bézier segments in the same
/// coordinate space as `node_pos` (screen/canvas-local). The loop points away
/// from the node's other incident edges (defaulting to up when the node has no
/// other edges or the average direction is zero).
pub(crate) fn self_loop_path<N, E>(
    node: NodeId,
    node_pos: Vec2,
    graph: &Graph<N, E>,
    node_position: &dyn Fn(NodeId) -> Option<Vec2>,
    viewport: &Viewport,
    style: &GraphStyle,
) -> Vec<Bezier> {
    // Local frame with up = (0, -1), right = (1, 0), node center at origin.
    // The node is the apex (tip) of the onigiri; the wide base sits away from
    // the node. The two sides leave and re-enter the node at two distinct
    // points on the node edge, both pointing toward the node center, so the
    // start and end are visually separate.
    let r = style.node_radius;
    // Two points just outside the node's circumference, symmetric about the
    // up-axis, angled 30° from the up-axis so they are distinct and point at
    // the center. The small outward offset keeps the loop clear of the node.
    let r_out = r + 2.0;
    let start = Vec2::new(-r_out * 0.5, -r_out * 0.866);
    let end = Vec2::new(r_out * 0.5, -r_out * 0.866);
    // The base size follows the graph's zoom linearly so the loop stays
    // proportionate to the graph as it scales. The base is kept small so the
    // loop does not dominate the node.
    let scale = viewport.zoom();
    let base_height = 8.5 * scale;
    let base_half_width = 4.5 * scale;
    let base_left = Vec2::new(-base_half_width, -base_height);
    let base_right = Vec2::new(base_half_width, -base_height);
    let base_mid = Vec2::new(0.0, -base_height);

    // Average direction from the node to the other endpoints of its incident
    // edges; the onigiri points opposite that average.
    let mut dir = Vec2::new(0.0, -1.0);
    if let Some(incident) = graph.incident_edges(node) {
        let mut sum = Vec2::ZERO;
        let mut count = 0usize;
        for edge_id in incident {
            let Some(edge) = graph.edge(*edge_id) else {
                continue;
            };
            let other = if edge.source == node {
                edge.target
            } else {
                edge.source
            };
            if other == node {
                continue;
            }
            let Some(other_world) = node_position(other) else {
                continue;
            };
            let other_screen = viewport.world_to_screen(other_world);
            let delta = other_screen - node_pos;
            if delta.length_squared() > f32::EPSILON {
                sum += delta.normalize();
                count += 1;
            }
        }
        if count > 0 {
            let avg = sum / count as f32;
            if avg.length_squared() > f32::EPSILON {
                dir = -avg.normalize();
            }
        }
    }

    // Rotate the local frame so `up` maps to `dir`. For `dir = (dx, dy)` with
    // `up = (0, -1)`, the rotation maps local `(lx, ly)` to
    // `x' = lx * (-dy) - ly * dx`, `y' = lx * dx + ly * (-dy)`.
    let (dx, dy) = (dir.x, dir.y);
    let rotate = |p: Vec2| {
        let x = p.x * (-dy) - p.y * dx;
        let y = p.x * dx + p.y * (-dy);
        node_pos + Vec2::new(x, y)
    };

    vec![
        (rotate(start), rotate(base_left), rotate(base_mid)),
        (rotate(base_mid), rotate(base_right), rotate(end)),
    ]
}

fn point_in_bounds(p: Vec2, bounds: &crate::viewport::WorldBounds, margin: f32) -> bool {
    p.x >= bounds.min.x - margin
        && p.x <= bounds.max.x + margin
        && p.y >= bounds.min.y - margin
        && p.y <= bounds.max.y + margin
}

/// Whether the axis-aligned box `(min, max)` intersects the visible bounds
/// (expanded by `margin`).
fn bounds_intersect(
    bounds: &crate::viewport::WorldBounds,
    margin: f32,
    min: Vec2,
    max: Vec2,
) -> bool {
    min.x <= bounds.max.x + margin
        && max.x >= bounds.min.x - margin
        && min.y <= bounds.max.y + margin
        && max.y >= bounds.min.y - margin
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

    fn no_clusters() -> impl Fn(NodeId) -> Option<(Vec2, f32)> {
        |_id| None
    }

    fn ctx<'a>(
        index: usize,
        signed_density: f32,
        zoom: f32,
        obstacles: &[Vec2],
        parallel: &'a [Option<(usize, usize)>],
    ) -> EdgeCurveContext<'a> {
        let grid = Box::leak(Box::new(ObstacleGrid::new(obstacles, 42.0)));
        EdgeCurveContext {
            index,
            signed_density,
            has_reverse: &[false],
            parallel,
            zoom,
            obstacles: grid,
            node_radius: 6.0,
        }
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
            node_cluster_center: &no_clusters(),
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
    fn keeps_curved_edge_whose_curve_crosses_viewport() {
        // Two nodes are both far outside the viewport, but a parallel edge
        // between them curves through the visible area. The edge must be kept
        // even though both endpoints are off-screen.
        let mut g: Graph<(), ()> = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Directed, ());
        g.add_edge(a, b, EdgeDirection::Directed, ());

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

        // Both nodes are far to the left and right, but the fanned curve
        // control point passes through the viewport.
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(-1000.0, 0.0))
            } else if id == b {
                Some(Vec2::new(1000.0, 0.0))
            } else {
                None
            }
        };

        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_cluster_center: &no_clusters(),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &style,
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        // The parallel edges curve through the viewport, so they are kept.
        assert_eq!(frame.edges.len(), 2);
    }

    #[test]
    fn keeps_straight_edge_whose_segment_crosses_viewport() {
        // A single (straight) edge whose endpoints are both outside the
        // viewport but whose segment passes through it must be kept.
        let mut g: Graph<(), ()> = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Directed, ());

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

        // The segment from (-1000, 0) to (1000, 0) passes through the viewport.
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(-1000.0, 0.0))
            } else if id == b {
                Some(Vec2::new(1000.0, 0.0))
            } else {
                None
            }
        };

        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_cluster_center: &no_clusters(),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &style,
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
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
            node_cluster_center: &no_clusters(),
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
            node_cluster_center: &no_clusters(),
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
            node_cluster_center: &no_clusters(),
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
            node_cluster_center: &no_clusters(),
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
            node_cluster_center: &no_clusters(),
            node_label: &no_labels(),
            edge_label: &edge_labels,
            viewport: &vp,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        assert_eq!(frame.edge_labels.len(), 1);
        assert_eq!(frame.edge_labels[0].text, "knows");
        let pos = frame.edge_labels[0].position;
        // A lone edge with no neighbors is straight, so the label sits at the
        // straight midpoint.
        assert!(
            (pos - Vec2::new(55.0, 50.0)).length() < 1e-3,
            "label should sit at the edge midpoint, got {pos:?}"
        );
    }

    #[test]
    fn single_edge_with_no_neighbors_is_straight() {
        let mut g: Graph<(), ()> = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Directed, ());
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else if id == b {
                Some(Vec2::new(10.0, 0.0))
            } else {
                None
            }
        };
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(100.0, 100.0));
        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_cluster_center: &no_clusters(),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        assert_eq!(frame.edges.len(), 1);
        assert_eq!(frame.edges[0].path.len(), 1, "single edge is one segment");
        // A lone edge with no neighbors has zero density, so it is straight: its
        // control point is the straight midpoint.
        let (p0, control, p2) = frame.edges[0].path[0];
        let mid = (p0 + p2) * 0.5;
        assert!(
            (control - mid).length() < 1e-3,
            "lone edge should be straight, control = {control:?}"
        );
    }

    #[test]
    fn overlapping_nodes_skip_degenerate_edge() {
        // Two nodes close enough that their boundary circles overlap (distance
        // less than 2 * (radius + gap)). The edge between them trims to a
        // degenerate curve; it must be skipped rather than producing non-finite
        // geometry that would crash the paint layer.
        let mut g: Graph<(), ()> = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Directed, ());
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else if id == b {
                Some(Vec2::new(2.0, 0.0))
            } else {
                None
            }
        };
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(100.0, 100.0));
        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_cluster_center: &no_clusters(),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        assert!(
            frame.edges.is_empty(),
            "degenerate edge must be skipped, got {} edges",
            frame.edges.len()
        );
    }

    #[test]
    fn parallel_edges_get_fanned_control_points() {
        let mut g: Graph<(), ()> = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Directed, ());
        g.add_edge(a, b, EdgeDirection::Directed, ());
        g.add_edge(a, b, EdgeDirection::Directed, ());
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else if id == b {
                Some(Vec2::new(10.0, 0.0))
            } else {
                None
            }
        };
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(100.0, 100.0));
        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_cluster_center: &no_clusters(),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        assert_eq!(frame.edges.len(), 3);
        // Each parallel edge is a single curved segment; the control points
        // (p1 of each segment) should be distinct and fanned vertically.
        let controls: Vec<Vec2> = frame.edges.iter().map(|e| e.path[0].1).collect();
        assert!(
            controls.iter().all(|c| c.is_finite()),
            "parallel edges must curve"
        );
        let ys: Vec<f32> = controls.iter().map(|c| c.y).collect();
        assert!(
            ys[0] != ys[1] && ys[1] != ys[2],
            "control points must be fanned"
        );
    }

    #[test]
    fn self_loop_gets_loop_control_point() {
        let mut g: Graph<(), ()> = Graph::new();
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
        vp.set_size(Vec2::new(100.0, 100.0));
        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_cluster_center: &no_clusters(),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        assert_eq!(frame.edges.len(), 1);
        let edge = &frame.edges[0];
        let path = &edge.path;
        assert_eq!(path.len(), 2, "onigiri has two segments");
        // The base center (p2 of the first segment) is above the node.
        let base = path[0].2;
        assert!(base.y < edge.source.y, "loop should be above the node");
    }

    #[test]
    fn self_loop_label_carries_its_path() {
        // A self-loop's label must carry the loop's path so it can slide along
        // it to avoid collisions, just like a non-loop edge label.
        let mut g: Graph<(), &str> = Graph::new();
        let a = g.add_node(());
        g.add_edge(a, a, EdgeDirection::Directed, "loop");
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
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
            node_cluster_center: &no_clusters(),
            node_label: &no_labels(),
            edge_label: &edge_labels,
            viewport: &vp,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        assert_eq!(frame.edge_labels.len(), 1);
        let label = &frame.edge_labels[0];
        assert!(
            !label.path.is_empty(),
            "self-loop label must carry its path for collision avoidance"
        );
        assert_eq!(label.t, 0.5, "self-loop label starts at the base center");
    }

    #[test]
    fn self_loop_points_up_without_other_edges() {
        let mut g: Graph<(), ()> = Graph::new();
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
        vp.set_size(Vec2::new(100.0, 100.0));
        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_cluster_center: &no_clusters(),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        let path = &frame.edges[0].path;
        let base = path[0].2;
        // With no other edges the onigiri points straight up: base directly
        // above the node center.
        assert!(
            (base.x - frame.edges[0].source.x).abs() < 1e-3,
            "base should be centered above the node"
        );
        assert!(base.y < frame.edges[0].source.y);
    }

    #[test]
    fn self_loop_points_away_from_other_edge() {
        let mut g: Graph<(), ()> = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, a, EdgeDirection::Directed, ());
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
        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_cluster_center: &no_clusters(),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        // The self-loop is the edge with source == target (path has 2 segments).
        let self_edge = frame
            .edges
            .iter()
            .find(|e| e.path.len() > 1)
            .expect("self-loop edge present");
        let path = &self_edge.path;
        let base = path[0].2;
        // The other edge points right (+x), so the onigiri base points left (-x).
        assert!(
            base.x < self_edge.source.x,
            "onigiri should point away from the other edge"
        );
    }

    #[test]
    fn self_loop_start_and_end_are_distinct_and_point_at_center() {
        let mut g: Graph<(), ()> = Graph::new();
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
        vp.set_size(Vec2::new(100.0, 100.0));
        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_cluster_center: &no_clusters(),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        let path = &frame.edges[0].path;
        let start = path[0].0;
        let end = path[1].2;
        let center = frame.edges[0].source;
        // The start and end are distinct points on the node edge.
        assert!(
            (start - end).length() > 1e-3,
            "start and end should be distinct"
        );
        // Both lie just outside the node circumference (distance ~= radius + 2).
        let radius = GraphStyle::default().node_radius;
        for p in [start, end] {
            let d = (p - center).length();
            assert!(
                (d - (radius + 2.0)).abs() < 1e-2,
                "endpoint should sit just outside the node edge, got {d}"
            );
        }
        // Both point toward the node center: the vector from the endpoint to
        // the center is roughly opposite the outward direction.
        for p in [start, end] {
            let to_center = (center - p).normalize();
            let outward = (p - center).normalize();
            assert!(
                to_center.dot(outward) < -0.9,
                "endpoint should point toward the node center"
            );
        }
    }

    #[test]
    fn self_loop_scales_with_zoom() {
        let mut g: Graph<(), ()> = Graph::new();
        let a = g.add_node(());
        g.add_edge(a, a, EdgeDirection::Directed, ());
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else {
                None
            }
        };
        let style = GraphStyle::default();

        let base_height = |vp: &Viewport| {
            let frame = build_paint_frame(PaintFrameInput {
                graph: &g,
                node_position: &positions,
                node_cluster_center: &no_clusters(),
                node_label: &no_labels(),
                edge_label: &no_edge_labels(),
                viewport: vp,
                style: &style,
                selection: &Selection::new(),
                hover: &Hover::default(),
            });
            let path = &frame.edges[0].path;
            let base = path[0].2;
            (frame.edges[0].source.y - base.y).abs()
        };

        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(200.0, 200.0));
        // Fit a 200x200 world bounds -> zoom 1.0.
        vp.fit_bounds(
            crate::viewport::WorldBounds {
                min: Vec2::new(-100.0, -100.0),
                max: Vec2::new(100.0, 100.0),
            },
            0.0,
        );
        let h1 = base_height(&vp);
        // Fit a 100x100 world bounds -> zoom 2.0.
        vp.fit_bounds(
            crate::viewport::WorldBounds {
                min: Vec2::new(-50.0, -50.0),
                max: Vec2::new(50.0, 50.0),
            },
            0.0,
        );
        let h2 = base_height(&vp);
        // At higher zoom the loop is larger, tracking the graph scale.
        assert!(
            h2 > h1,
            "self-loop should grow with zoom (h1={h1}, h2={h2})"
        );
    }

    #[test]
    fn edge_bows_away_from_neighbor_edge() {
        // A horizontal edge a->b with a neighbor edge whose midpoint lies on
        // the left (signed_density > 0) bows right, away from the density.
        let mut g: Graph<(), ()> = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        let c = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Directed, ());
        g.add_edge(a, c, EdgeDirection::Directed, ());
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else if id == b {
                Some(Vec2::new(100.0, 0.0))
            } else if id == c {
                Some(Vec2::new(50.0, 30.0))
            } else {
                None
            }
        };
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(200.0, 200.0));
        vp.fit_bounds(
            crate::viewport::WorldBounds {
                min: Vec2::new(-10.0, -10.0),
                max: Vec2::new(110.0, 40.0),
            },
            0.0,
        );
        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_cluster_center: &no_clusters(),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        // Find the horizontal edge a->b by its endpoints.
        let horizontal = frame
            .edges
            .iter()
            .find(|e| (e.source.y - e.target.y).abs() < 1e-3)
            .expect("horizontal edge exists");
        let (_, control, _) = horizontal.path[0];
        // The neighbor edge's midpoint (25, 15) is on the left of a->b
        // (direction +x, normal +y, dot > 0), so a->b bows right (control.y > 0),
        // away from the neighbor.
        assert!(
            control.y > 0.0,
            "edge should bow away from the neighbor on its left, control = {control:?}"
        );
    }

    #[test]
    fn edge_bow_grows_with_density_difference() {
        // The bow magnitude grows with the signed density difference. A lone
        // edge (density 0) is straight; a neighbor on the left (density +1)
        // bows right.
        let source = Vec2::new(0.0, 0.0);
        let target = Vec2::new(100.0, 0.0);
        let lone = edge_control_point(source, target, &ctx(0, 0.0, 1.0, &[], &[None]), None);
        let with_neighbor =
            edge_control_point(source, target, &ctx(0, 1.0, 1.0, &[], &[None]), None);
        // A neighbor on the left (signed_density +1) pushes the bow right
        // (negative y, since normal +y is the left side of direction +x).
        assert!(
            with_neighbor.y < lone.y,
            "bow should grow with density difference (lone={lone:?}, neighbor={with_neighbor:?})"
        );
    }

    #[test]
    fn edge_bow_is_stable_under_zoom() {
        // The bow is a fraction of the edge length, so the curve shape is the
        // same at any zoom. Compare the control point's offset from the straight
        // midpoint, normalized by the edge length, at two zooms.
        let bow_ratio = |source: Vec2, target: Vec2, zoom: f32| {
            let control =
                edge_control_point(source, target, &ctx(0, 0.0, zoom, &[], &[None]), None);
            let mid = (source + target) * 0.5;
            (control - mid).length() / (target - source).length()
        };
        // Zoom 1: world a(0,0) b(100,0) maps to screen (0,0) and (100,0).
        let r1 = bow_ratio(Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0), 1.0);
        // Zoom 2: the same world edge maps to screen (0,0) and (200,0).
        let r2 = bow_ratio(Vec2::new(0.0, 0.0), Vec2::new(200.0, 0.0), 1.0);
        assert!(
            (r1 - r2).abs() < 1e-3,
            "bow ratio should be zoom-invariant (r1={r1}, r2={r2})"
        );
    }

    #[test]
    fn parallel_curvature_drops_as_node_distance_grows() {
        // The parallel spacing scales sub-linearly with apparent length (power
        // < 1), so the sagitta grows more slowly than the chord and curvature
        // drops as the node distance grows.
        let sagitta = |source: Vec2, target: Vec2| {
            let control = edge_control_point(
                source,
                target,
                &ctx(0, 0.0, 1.0, &[], &[Some((0, 2))]),
                None,
            );
            let mid = (source + target) * 0.5;
            (control - mid).length()
        };
        // Short edge: a(0,0) b(100,0).
        let short = sagitta(Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0));
        // Long edge: a(0,0) b(200,0).
        let long = sagitta(Vec2::new(0.0, 0.0), Vec2::new(200.0, 0.0));
        // The sagitta varies only slightly with length.
        assert!(
            (short - long).abs() < 20.0,
            "sagitta should vary only slightly (short={short}, long={long})"
        );
        // Curvature is sagitta / length, so it drops for the longer edge.
        assert!(
            long / 200.0 < short / 100.0,
            "curvature should drop as node distance grows"
        );
    }

    #[test]
    fn cluster_edge_bows_outward_from_center() {
        // An edge whose endpoints share a cluster center bows outward from that
        // center, independent of local density, so the cluster reads as a circle
        // even for long edges.
        let center = Vec2::new(0.0, 0.0);
        let radius = 100.0;
        // Two nodes on a circle of radius 100 around the origin: top and right.
        let source = Vec2::new(0.0, -100.0);
        let target = Vec2::new(100.0, 0.0);
        let control = edge_control_point(
            source,
            target,
            &ctx(0, 0.0, 1.0, &[], &[None]),
            Some((center, radius)),
        );
        let mid = (source + target) * 0.5;
        // The control point must be farther from the center than the chord
        // midpoint, i.e. bowed outward.
        assert!(
            control.length() > mid.length(),
            "cluster edge should bow outward (control={control:?}, mid={mid:?})"
        );
        // Without a cluster center the same edge is straight (density 0).
        let straight = edge_control_point(source, target, &ctx(0, 0.0, 1.0, &[], &[None]), None);
        assert!(
            (straight - mid).length() < 1e-3,
            "unclustered edge should be straight"
        );
    }

    #[test]
    fn edge_bows_away_from_obstacle_node() {
        // An edge from (0,0) to (100,0) with an obstacle node near its midpoint
        // must bow its control point away from the obstacle.
        let source = Vec2::new(0.0, 0.0);
        let target = Vec2::new(100.0, 0.0);
        let obstacle = Vec2::new(50.0, 0.0);
        let control = edge_control_point(
            source,
            target,
            &ctx(0, 0.0, 1.0, &[obstacle], &[None]),
            None,
        );
        // The control point must be pushed off the chord (y != 0) away from the
        // obstacle, which sits on the chord.
        assert!(
            control.y.abs() > 1e-3,
            "edge should bow away from the obstacle (control={control:?})"
        );
    }

    #[test]
    fn parallel_spacing_varies_with_zoom_and_length() {
        // The spacing scales with the edge's apparent length (world length times
        // zoom): a longer apparent distance yields a wider spacing.
        let control = |source: Vec2, target: Vec2, zoom: f32| {
            edge_control_point(
                source,
                target,
                &ctx(0, 0.0, zoom, &[], &[Some((0, 2))]),
                None,
            )
        };
        // Base length and zoom.
        let base = control(Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0), 1.0);
        // Higher zoom widens the spacing (larger apparent length).
        let high_zoom = control(Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0), 2.0);
        assert!(
            high_zoom.y.abs() > base.y.abs(),
            "higher zoom should widen the spacing (base={}, high={})",
            base.y,
            high_zoom.y
        );
        // Longer edge widens the spacing (larger apparent length).
        let long = control(Vec2::new(0.0, 0.0), Vec2::new(200.0, 0.0), 1.0);
        assert!(
            long.y.abs() > base.y.abs(),
            "longer edge should widen the spacing (base={}, long={})",
            base.y,
            long.y
        );
    }

    #[test]
    fn self_loop_counts_toward_neighbor_density_but_keeps_its_shape() {
        // A self-loop on node a contributes to the local density of a nearby
        // edge a->b, pushing that edge's bow away. The self-loop's own onigiri
        // shape is unchanged (its curvature does not depend on density).
        let mut g: Graph<(), ()> = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, a, EdgeDirection::Directed, ());
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
            crate::viewport::WorldBounds {
                min: Vec2::new(-10.0, -10.0),
                max: Vec2::new(110.0, 10.0),
            },
            0.0,
        );
        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_cluster_center: &no_clusters(),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
        });
        assert_eq!(frame.edges.len(), 2);
        // The self-loop is the two-segment onigiri; the a->b edge is the single
        // curved segment.
        let self_loop = frame
            .edges
            .iter()
            .find(|e| e.path.len() == 2)
            .expect("self-loop has two onigiri segments");
        let ab = frame
            .edges
            .iter()
            .find(|e| e.path.len() == 1)
            .expect("a->b is a single segment");
        // The self-loop's midpoint is at node a (0, 0), which is on the left of
        // a->b (direction +x, normal +y, dot > 0), so a->b bows right
        // (control.y > 0), away from the self-loop's density.
        let (_, ab_control, _) = ab.path[0];
        assert!(
            ab_control.y > 0.0,
            "a->b should bow away from the self-loop's density, control = {ab_control:?}"
        );
        // The self-loop keeps its onigiri shape: its base center (p2 of the
        // first segment) sits away from the node center, unchanged by density.
        let base = self_loop.path[0].2;
        assert!(
            (base - self_loop.source).length() > 1.0,
            "self-loop should keep its onigiri shape, base = {base:?}"
        );
    }

    #[test]
    fn density_weights_closer_neighbors_more() {
        // A neighbor edge on the left contributes more when it is closer. Two
        // edges on the left at different distances yield different signed
        // densities, so the bow magnitude differs.
        let radius = 40.0;
        // Neighbor at distance 10 on the left.
        let near = signed_densities(
            &[Vec2::new(0.0, 0.0), Vec2::new(0.0, 10.0)],
            &[Vec2::new(0.0, 1.0), Vec2::new(0.0, 1.0)],
            radius,
        );
        // Neighbor at distance 30 on the left.
        let far = signed_densities(
            &[Vec2::new(0.0, 0.0), Vec2::new(0.0, 30.0)],
            &[Vec2::new(0.0, 1.0), Vec2::new(0.0, 1.0)],
            radius,
        );
        // Both are positive (left side); the closer one contributes more.
        assert!(near[0] > 0.0 && far[0] > 0.0);
        assert!(
            near[0] > far[0],
            "closer neighbor should contribute more (near={}, far={})",
            near[0],
            far[0]
        );
    }

    #[test]
    fn density_balances_left_and_right_neighbors() {
        // A neighbor on the left and a neighbor on the right at the same
        // distance cancel out, so the signed density is near zero.
        let radius = 40.0;
        let densities = signed_densities(
            &[
                Vec2::new(0.0, 0.0),
                Vec2::new(0.0, 10.0),
                Vec2::new(0.0, -10.0),
            ],
            &[
                Vec2::new(0.0, 1.0),
                Vec2::new(0.0, 1.0),
                Vec2::new(0.0, 1.0),
            ],
            radius,
        );
        assert!(
            densities[0].abs() < 1e-3,
            "balanced neighbors should cancel, got {}",
            densities[0]
        );
    }

    #[test]
    fn density_is_continuous_across_edge_axis() {
        // As a neighbor edge's midpoint crosses the edge's axis (the normal
        // component passes through zero), the signed density must transition
        // smoothly rather than jump. This is what keeps edges from jittering
        // while a node is dragged. The neighbor sits at an along-axis distance
        // of 10 with a small perpendicular offset that flips sign.
        let radius = 40.0;
        let normals = [Vec2::new(0.0, 1.0), Vec2::new(0.0, 1.0)];
        // Neighbor just on the left of the axis.
        let left = signed_densities(
            &[Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.1)],
            &normals,
            radius,
        );
        // Neighbor just on the right of the axis.
        let right = signed_densities(
            &[Vec2::new(0.0, 0.0), Vec2::new(10.0, -0.1)],
            &normals,
            radius,
        );
        // The two densities are near zero and close to each other (no jump).
        assert!(
            (left[0] - right[0]).abs() < 0.02,
            "density should be continuous across the axis (left={}, right={})",
            left[0],
            right[0]
        );
        // The sign flips smoothly through zero.
        assert!(left[0] > 0.0 && right[0] < 0.0);
    }
}
