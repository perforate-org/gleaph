//! Paint frame (§18.2).
//!
//! A [`PaintFrame`] is an intermediate frame representation containing only the
//! information required for the current paint: visible nodes and edges already
//! transformed to canvas-local pixels, plus interaction highlights. This
//! separates graph and scene state from rendering mechanics. The `GraphView`
//! boundary owns the separate conversion between canvas-local and window-space
//! GPUI coordinates.

use std::hash::BuildHasher;

use glam::Vec2;

use crate::graph::{Edge, EdgeDirection, EdgeId, Graph, NodeId};
use crate::interaction::{Hover, Selection};
use crate::runtime::{GraphRuntime, SyncedGraphRuntime};
use crate::style::GraphStyle;
use crate::viewport::Viewport;

/// A quadratic Bézier curve `(p0, p1, p2)`.
pub type Bezier = (Vec2, Vec2, Vec2);

/// A per-element query-overlay category, independent of selection and hover.
///
/// A query result is expressed as an overlay over the persistent graph scene
/// rather than a replacement graph. `None` keeps the base style; `Dimmed`
/// renders the element subdued; `Emphasized`/`Accent` render it prominent.
/// This is orthogonal to the element's `selected`/`hovered` state so a result
/// the user also selects can be shown as both simultaneously (§10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverlayCategory {
    /// No query overlay applies; the base style is used.
    None,
    /// The element is unrelated context and is rendered subdued.
    Dimmed,
    /// The element participates in the active query result and is emphasized.
    Emphasized,
    /// The element is the principal returned result and is rendered most
    /// prominently.
    Accent,
}

/// A per-node overlay category lookup. `None` is equivalent to
/// [`OverlayCategory::None`] and keeps the base style.
pub type NodeOverlay = dyn Fn(NodeId) -> OverlayCategory;

/// A per-edge overlay category lookup. `None` is equivalent to
/// [`OverlayCategory::None`] and keeps the base style.
pub type EdgeOverlay = dyn Fn(EdgeId) -> OverlayCategory;

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
    /// Query-overlay category for the node, independent of `selected`/`hovered`.
    pub overlay: OverlayCategory,
    /// Whether the node renders simplified (fill only, no stroke) under node
    /// LOD. True when the node's on-screen diameter is below
    /// `GraphStyle::node_simplify_threshold`.
    pub simplified: bool,
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
    /// Query-overlay category for the edge, independent of `selected`/`hovered`.
    pub overlay: OverlayCategory,
    /// Whether the directed edge's arrowhead should be omitted under arrow LOD.
    /// True only for a directed, non-self-loop edge whose on-screen chord is
    /// below `GraphStyle::edge_arrow_min_length`. See
    /// [`crate::style::GraphStyle::edge_arrow_min_length`]. Self-loops always
    /// keep their arrow; the flag is `false` for them and for undirected edges.
    pub omit_arrow: bool,
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
    /// Optional per-node query-overlay category. When `None`, every node keeps
    /// its base style (equivalent to an all-`None` overlay).
    pub node_overlay: Option<&'a NodeOverlay>,
    /// Optional per-edge query-overlay category. When `None`, every edge keeps
    /// its base style (equivalent to an all-`None` overlay).
    pub edge_overlay: Option<&'a EdgeOverlay>,
}

/// Inputs to [`build_indexed_paint_frame`].
///
/// Indexed rendering accepts only a scene/runtime synchronization proof. The
/// graph, positions, and cluster geometry are therefore all resolved from the
/// same borrowed scene snapshot as the spatial index.
pub struct IndexedPaintFrameInput<'a, 'scene, NK, EK, N, E, S = crate::hash::DefaultBuildHasher>
where
    S: BuildHasher + Default + Clone,
{
    /// Proof returned by [`crate::scene::GraphScene::sync_runtime`].
    pub synced: &'a SyncedGraphRuntime<'scene, NK, EK, N, E, S>,
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
    /// Optional per-node query-overlay category. When `None`, every node keeps
    /// the base style (equivalent to an all-`None` overlay).
    pub node_overlay: Option<&'a NodeOverlay>,
    /// Optional per-edge query-overlay category. When `None`, every edge keeps
    /// the base style (equivalent to an all-`None` overlay).
    pub edge_overlay: Option<&'a EdgeOverlay>,
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
    build_paint_frame_with_runtime::<N, E, crate::hash::DefaultBuildHasher>(input, None)
}

/// Build a paint frame using synchronized scene-owned spatial-index state.
pub fn build_indexed_paint_frame<NK, EK, N, E, S>(
    input: IndexedPaintFrameInput<'_, '_, NK, EK, N, E, S>,
) -> PaintFrame
where
    NK: Eq + std::hash::Hash,
    EK: Eq + std::hash::Hash,
    S: BuildHasher + Default + Clone,
{
    let IndexedPaintFrameInput {
        synced,
        node_label,
        edge_label,
        viewport,
        style,
        selection,
        hover,
        node_overlay,
        edge_overlay,
    } = input;
    let scene = synced.scene;
    build_paint_frame_with_runtime(
        PaintFrameInput {
            graph: scene.graph(),
            node_position: &|id| scene.node_position(id),
            node_cluster_center: &|id| scene.node_cluster_center(id),
            node_label,
            edge_label,
            viewport,
            style,
            selection,
            hover,
            node_overlay,
            edge_overlay,
        },
        Some(synced.runtime),
    )
}

fn build_paint_frame_with_runtime<N, E, S>(
    input: PaintFrameInput<'_, N, E>,
    runtime: Option<&GraphRuntime<S>>,
) -> PaintFrame
where
    S: BuildHasher + Default + Clone,
{
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
        node_overlay,
        edge_overlay,
    } = input;
    let node_overlay = node_overlay.unwrap_or(&|_| OverlayCategory::None);
    let edge_overlay = edge_overlay.unwrap_or(&|_| OverlayCategory::None);
    let visible = viewport.visible_world_bounds();
    let margin = style.node_radius * 2.0;
    let zoom = viewport.zoom().max(f32::EPSILON);

    let mut frame = PaintFrame::new();

    // A degenerate (zero-area) viewport has nothing visible.
    if visible.is_empty() {
        return frame;
    }

    // Iterate the visible node ids: from the spatial index when supplied,
    // otherwise a linear scan of the whole graph. The index returns a superset
    // of the nodes inside the visible bounds (a node in a boundary cell may lie
    // just outside), so the precise point-in-bounds test below still runs.
    let node_ids: Vec<NodeId> = match runtime {
        Some(rt) => rt.visible_nodes(&visible, margin),
        None => graph.nodes().map(|(id, _)| id).collect(),
    };
    for id in &node_ids {
        let Some(world) = node_position(*id) else {
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
        let Some(node) = graph.node(*id) else {
            continue;
        };
        // Node LOD: a node whose on-screen diameter is at or below
        // `node_simplify_threshold` renders fill-only (no stroke). The diameter
        // is `2 * node_radius` in screen pixels.
        // The simplification LOD judges the true on-screen diameter: the
        // style's minimum screen radius only lifts the drawn marker, it must
        // not keep a shrunken node from dropping its sub-pixel stroke.
        let simplified = style.node_simplify_threshold > 0.0
            && style.node_radius * 2.0 * zoom <= style.node_simplify_threshold;
        frame.nodes.push(PaintNode {
            id: *id,
            position: viewport.world_to_screen(world),
            // One shared clamped screen radius keeps markers visible at deep
            // zoom-out and label-scaled past the ceiling (see
            // `GraphStyle::node_screen_radius`).
            radius: style.node_screen_radius(zoom),
            selected: selection.contains_node(*id),
            hovered: hover.node == Some(*id),
            overlay: node_overlay(*id),
            simplified,
        });
        if let Some(text) = node_label(*id, &node.data) {
            frame.labels.push(PaintLabel {
                position: viewport.world_to_screen(world),
                text,
            });
        }
    }

    // An empty field used only for culling: the cull test only needs the
    // curve's bounding box, and avoidance now happens later in world space.
    // The same empty field also serves as the obstacle context when every
    // visible edge is a straight-line-LOD edge (which never reads obstacles),
    // so no obstacle field is built in the common zoomed-out case.
    //
    // Nodes are world-sized, so the obstacle clearance is a plain world
    // length and curve shapes are fully invariant under both panning and
    // zooming. The field's collection window widens by one clearance radius
    // beyond the node-cull margin, so a node drifting across the boundary is
    // already fully present (or fully absent) for every edge that can see it
    // and shapes do not pop as the view pans.
    let obstacle_radius = style.node_radius * 2.0 + OBSTACLE_RADIUS;
    let field_margin = margin + obstacle_radius;
    let empty_obstacle_field = ObstacleField::new(&[], obstacle_radius);

    // The zoom-invariant per-edge preprocessing comes from the runtime when
    // supplied (borrowed, no copy); otherwise build it here (linear scan). It
    // holds the candidate edges, parallel groups, midpoints/normals, and the
    // density grid.
    let prep: std::borrow::Cow<'_, crate::runtime::EdgePrep<S>> = match runtime {
        Some(rt) => std::borrow::Cow::Borrowed(rt.edges()),
        None => std::borrow::Cow::Owned(crate::runtime::build_edge_prep(
            graph,
            node_position,
            S::default(),
        )),
    };
    let has_reverse = &prep.has_reverse;
    let parallel = &prep.parallel;
    let midpoints = &prep.midpoints;
    let normals = &prep.normals;
    let density_grid = &prep.density_grid;

    let mut visible_edges: Vec<(usize, EdgeId, &Edge<E>, Vec2, Vec2)> = Vec::new();
    // When a spatial index is supplied, iterate only the candidate indices it
    // reports as near the visible region (a superset, so no visible edge is
    // missed). Without an index, every edge is a candidate.
    let candidate_indices: Vec<usize> = match runtime {
        Some(rt) => rt.visible_edge_candidates(&visible, margin),
        None => (0..prep.edge_ids.len()).collect(),
    };
    for &index in &candidate_indices {
        let id = prep.edge_ids[index];
        let edge = graph.edge(id).expect("edge exists");
        let source_world = prep.source[index];
        let target_world = prep.target[index];
        // Cull edges whose curve's bounding box is entirely outside the visible
        // bounds. A curved edge may pass through the view even when both
        // endpoints are outside it, so the control point is included in the
        // bounds test.
        let source_visible = point_in_bounds(source_world, &visible, margin);
        let target_visible = point_in_bounds(target_world, &visible, margin);
        if !source_visible && !target_visible {
            // Both endpoints are outside; keep the edge only if its curve
            // (including the control point) still crosses the visible bounds.
            // Self-loop status is a graph-topology fact, not a consequence of
            // two distinct nodes currently sharing a position.
            let is_self_loop = edge.source == edge.target;
            let curve_visible = if is_self_loop {
                // A self-loop's onigiri path may extend well beyond the node,
                // so test the path's bounding box. The path is in screen
                // coordinates; convert its bounds back before comparing with
                // the world-space visible bounds.
                let path = self_loop_path(
                    edge.source,
                    viewport.world_to_screen(source_world),
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
                bounds_intersect(
                    &visible,
                    margin,
                    viewport.screen_to_world(min),
                    viewport.screen_to_world(max),
                )
            } else {
                let cluster_center =
                    shared_cluster_center(edge.source, edge.target, node_cluster_center);
                let (min, max) = edge_curve_bbox(
                    source_world,
                    target_world,
                    index,
                    has_reverse,
                    parallel,
                    cluster_center,
                    &empty_obstacle_field,
                );
                bounds_intersect(&visible, margin, min, max)
            };
            if !curve_visible {
                continue;
            }
        }
        visible_edges.push((
            index,
            id,
            edge,
            viewport.world_to_screen(source_world),
            viewport.world_to_screen(target_world),
        ));
    }

    // Compute the signed density only for the visible edges that will render
    // curved. Straight-line-LOD edges (below the screen-length threshold) skip
    // the control-point bow entirely, so they do not need a density value; this
    // keeps the pairwise density loop proportional to the curved-visible set
    // instead of every visible edge. The grid is built over every edge's
    // midpoint, so off-screen neighbors still count toward a visible edge's
    // density, but the pairwise loop runs only for the requested indices.
    let curved_indices: Vec<usize> = visible_edges
        .iter()
        .filter_map(|(index, _, edge, source, target)| {
            let is_self_loop = edge.source == edge.target;
            let is_straight = !is_self_loop && straight_edge_applies(*source, *target, style);
            (!is_straight).then_some(*index)
        })
        .collect();
    let signed_densities = signed_densities_for(
        density_grid,
        midpoints,
        normals,
        DENSITY_RADIUS,
        &curved_indices,
    );

    // Build the obstacle field only when at least one visible edge renders
    // curved, since straight-line-LOD edges never read it. In the zoomed-out
    // overview (every edge straight) this avoids building a field over every
    // visible node's position. When no edge is curved, the shared empty field
    // is used as the obstacle context.
    let obstacles_field: ObstacleField = if curved_indices.is_empty() {
        empty_obstacle_field
    } else {
        let mut obstacles_world: Vec<Vec2> = Vec::new();
        for id in &node_ids {
            let Some(world) = node_position(*id) else {
                continue;
            };
            if !point_in_bounds(world, &visible, field_margin) {
                continue;
            }
            obstacles_world.push(world);
        }
        ObstacleField::new(&obstacles_world, obstacle_radius)
    };

    for (candidate_index, id, edge, source, target) in visible_edges.iter() {
        let is_self_loop = edge.source == edge.target;
        // Membership must mirror the field's own collection window above:
        // an endpoint belongs to the field exactly when it passed the widened
        // filter. Curve-visible edges with endpoints beyond it read a field
        // without them.
        let endpoints_in_field = (
            point_in_bounds(prep.source[*candidate_index], &visible, field_margin),
            point_in_bounds(prep.target[*candidate_index], &visible, field_margin),
        );
        let path = edge_path(
            edge,
            &EdgeCurveContext {
                index: *candidate_index,
                signed_density: signed_densities[*candidate_index],
                has_reverse,
                parallel,
                obstacles: &obstacles_field,
                obstacle_radius,
                endpoints_in_field,
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
        // The candidate/index cull is intentionally conservative because the
        // exact density and obstacle context is known only now. For edges with
        // both endpoints outside the viewport, apply the actual rendered path
        // as the post-candidate predicate so a conservative max-bow bbox cannot
        // turn an off-screen edge into a painted primitive.
        let screen_bounds = crate::viewport::WorldBounds {
            min: Vec2::ZERO,
            max: viewport.size(),
        };
        let screen_margin = margin * viewport.zoom();
        let source_visible = point_in_bounds(*source, &screen_bounds, screen_margin);
        let target_visible = point_in_bounds(*target, &screen_bounds, screen_margin);
        if !source_visible && !target_visible {
            let path_visible = path.iter().any(|(p0, p1, p2)| {
                bounds_intersect(
                    &screen_bounds,
                    screen_margin,
                    p0.min(*p1).min(*p2),
                    p0.max(*p1).max(*p2),
                )
            });
            if !path_visible {
                continue;
            }
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
                // Normalize the normal so its y component is always upward.
                // This keeps labels on the same side of the edge regardless of
                // whether the edge points left or right.
                let normal = if let Some(len) = finite_chord_length(*source, *target) {
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
            overlay: edge_overlay(*id),
            omit_arrow: edge_arrow_omitted(*source, *target, edge.source == edge.target, style),
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
#[doc(hidden)]
pub const DENSITY_RADIUS: f32 = 40.0;
/// Base world-space spacing between parallel edges, at the reference edge
/// length. The actual spacing scales with the edge's world length: a shorter
/// edge yields a narrower spacing, a longer edge a wider spacing, so parallel
/// edges keep a consistent on-screen separation. The power is sub-linear so the
/// sagitta grows more slowly than the chord and curvature still drops as the
/// node distance grows. The spacing is zoom-invariant (it depends only on world
/// length, not on the viewport zoom) so the spatial index, the cull test, and
/// the drawn edge all agree at every zoom level.
const PARALLEL_SPACING: f32 = 60.0;
/// Reference edge length (world units) at which the spacing is the base value.
const PARALLEL_SPACING_REF_APPARENT: f32 = 100.0;
/// Power relating edge length to spacing. 0 keeps spacing constant; 1 makes
/// spacing proportional to edge length.
const PARALLEL_SPACING_POWER: f32 = 0.10;

/// Parallel-edge spacing in the current coordinate space. `coordinate_scale`
/// converts current-coordinate length to world length, then scales the spacing
/// back into the current coordinate space.
fn parallel_spacing(len: f32, coordinate_scale: f32) -> f32 {
    let world_len = len / coordinate_scale;
    coordinate_scale
        * PARALLEL_SPACING
        * (world_len / PARALLEL_SPACING_REF_APPARENT).powf(PARALLEL_SPACING_POWER)
}

/// Bow per unit of signed density difference, as a fraction of edge length.
const BOW_DENSITY: f32 = 0.20;
/// Upper bound on the bow as a fraction of edge length.
const BOW_MAX: f32 = 0.90;
/// Signed density value that reaches the `BOW_MAX` magnitude cap.
const MAX_SIGNED_DENSITY: f32 = BOW_MAX / BOW_DENSITY;
/// Extra clearance around an obstacle node, in world units, that an edge's
/// control point is pushed away from.
pub(crate) const OBSTACLE_RADIUS: f32 = 30.0;

/// Return a usable length for a coordinate-space chord.
///
/// A fixed epsilon is not a valid degeneracy rule here: a finite world-space
/// chord can be smaller than `f32::EPSILON` and become drawable after the
/// viewport transform. Only an exact zero or a non-finite length is rejected,
/// which keeps every normalization site finite without conflating coordinate
/// scale with graph topology.
pub(crate) fn finite_chord_length(source: Vec2, target: Vec2) -> Option<f32> {
    let dir = target - source;
    let len = dir.length();
    (dir.is_finite() && len.is_finite() && len > 0.0).then_some(len)
}
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

/// A rasterized obstacle repulsion field over node positions, so an edge
/// evaluates a fixed number of bilinear samples instead of scanning the nodes
/// near its chord.
///
/// The field stores the sum of compact quadratic kernels centered on every
/// obstacle: `w(d) = max(0, 1 - d/radius)^2` accumulated at surrounding cell
/// centers. [`apply_node_avoidance`] reads the field along an edge's chord and
/// derives one perpendicular push from local field value and gradient, so
/// per-edge cost is proportional to the fixed sample count only — never to the
/// number of nearby nodes, whatever the graph density.
///
/// The raster is anchored to a global world lattice: the origin snaps down to
/// a multiple of the cell size, so when the visible window moves its edges
/// slide along that lattice while every interior cell center keeps its world
/// position. A given world point therefore reads identical values regardless
/// of pan, and curve shapes cannot wobble as the view moves; only the zoom
/// level rescales the clearance (see [`apply_node_avoidance`]). Coordinates
/// are world coordinates and `radius` is a world-space length. Samples
/// outside the extent read zero; a pathologically wide extent grows the cell
/// size instead of the allocation, bounding memory while queries stay
/// correct. Splats accumulate in input order, row-major, so builds are
/// deterministic.
#[doc(hidden)]
pub struct ObstacleField {
    /// Position of the outer corner of cell (0, 0).
    origin: Vec2,
    cols: u32,
    rows: u32,
    cell: f32,
    data: Vec<f32>,
}

/// Upper bound on raster cells, so a hostile extent cannot balloon memory.
const OBSTACLE_FIELD_MAX_CELLS: usize = 1 << 19;
/// Chord sample count for field-based avoidance. Includes both endpoints and,
/// being odd, the midpoint where the control point starts.
const OBSTACLE_SAMPLES: usize = 7;
/// Side-probe offset as a fraction of the kernel radius.
const OBSTACLE_SIDE_PROBE: f32 = 0.25;
/// Gain of the sparse-side steering term, in units of the probed density
/// difference. Tuned so a clearly one-sided obstacle hands control to this
/// term over the fixed-side kick.
const OBSTACLE_STEER_GAIN: f32 = 3.0;
/// Fixed-side push where the field is dense but symmetric across the chord —
/// an obstacle sitting on it. The previous per-obstacle model flipped sides
/// discontinuously at zero perpendicular distance with `-normal` as the
/// non-negative default; the kick preserves that default side continuously.
const OBSTACLE_CENTER_KICK: f32 = 0.5;
/// Exponent of the symmetry gate on the fixed-side kick: `(min/max)^power`.
/// High power keeps the kick confined to genuinely straddling mass (an
/// obstacle sitting on or very near the chord); any clear side dominance
/// hands control to the steering term.
const OBSTACLE_KICK_SYMMETRY_POWER: i32 = 6;
/// Converts the dimensionless field response into world displacement.
/// Calibrated so a single obstacle centered on the chord deflects the control
/// point by roughly one influence radius before the length cap.
const OBSTACLE_FIELD_SCALE: f32 = 4.0;

#[doc(hidden)]
impl ObstacleField {
    /// Build the field over `obstacles` with the given kernel `radius`.
    #[doc(hidden)]
    pub fn new(obstacles: &[Vec2], radius: f32) -> Self {
        let empty = || Self {
            origin: Vec2::ZERO,
            cols: 0,
            rows: 0,
            cell: 0.0,
            data: Vec::new(),
        };
        if obstacles.is_empty() || radius <= 0.0 || !radius.is_finite() {
            return empty();
        }
        let mut min = Vec2::splat(f32::INFINITY);
        let mut max = Vec2::splat(f32::NEG_INFINITY);
        for &o in obstacles {
            min = min.min(o);
            max = max.max(o);
        }
        if !min.is_finite() {
            return empty();
        }
        // Expand by one radius so every full kernel fits inside the raster and
        // gradient probes just outside obstacle hulls stay in range.
        let min = min - Vec2::splat(radius);
        let max = max + Vec2::splat(radius);
        // Half a kernel radius per cell resolves the quadratic bump well
        // enough for bilinear sampling while bounding splat work.
        let mut cell = (radius * 0.5).max(1e-3);
        // Snap the origin down onto a global lattice of cell multiples so the
        // raster's phase never depends on which nodes happen to be visible,
        // then grow the cell until the snapped window fits the cap.
        let cap = OBSTACLE_FIELD_MAX_CELLS as u64;
        let mut origin;
        let mut cols;
        let mut rows;
        loop {
            origin = Vec2::new((min.x / cell).floor() * cell, (min.y / cell).floor() * cell);
            cols = (((max.x - origin.x) / cell).floor() as u64) + 1;
            rows = (((max.y - origin.y) / cell).floor() as u64) + 1;
            if cols.saturating_mul(rows) <= cap {
                break;
            }
            cell *= 1.1;
        }
        let cols = cols.min(u32::MAX as u64) as u32;
        let rows = rows.min(u32::MAX as u64) as u32;
        let mut field = Self {
            origin,
            cols,
            rows,
            cell,
            data: vec![0.0; cols as usize * rows as usize],
        };
        for &o in obstacles {
            field.splat(o, radius);
        }
        field
    }

    /// Accumulate this point's kernel into every cell center within range.
    fn splat(&mut self, point: Vec2, radius: f32) {
        let lo_x = (((point.x - radius - self.origin.x) / self.cell).floor() as i64)
            .clamp(0, self.cols as i64 - 1);
        let hi_x = (((point.x + radius - self.origin.x) / self.cell).floor() as i64)
            .clamp(0, self.cols as i64 - 1);
        let lo_y = (((point.y - radius - self.origin.y) / self.cell).floor() as i64)
            .clamp(0, self.rows as i64 - 1);
        let hi_y = (((point.y + radius - self.origin.y) / self.cell).floor() as i64)
            .clamp(0, self.rows as i64 - 1);
        for row in lo_y..=hi_y {
            for col in lo_x..=hi_x {
                let center = Vec2::new(
                    self.origin.x + (col as f32 + 0.5) * self.cell,
                    self.origin.y + (row as f32 + 0.5) * self.cell,
                );
                let dist = center.distance(point);
                let base = 1.0 - dist / radius;
                if base > 0.0 {
                    self.data[(row as usize) * self.cols as usize + col as usize] += base * base;
                }
            }
        }
    }

    /// Whether the field holds no obstacles.
    pub(crate) fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Bilinear field value at `point`; outside the extent this is zero.
    fn sample(&self, point: Vec2) -> f32 {
        if self.data.is_empty() {
            return 0.0;
        }
        let g = (point - self.origin) / self.cell - Vec2::splat(0.5);
        let x0 = g.x.floor();
        let y0 = g.y.floor();
        let fx = g.x - x0;
        let fy = g.y - y0;
        let (xi, yi) = (x0 as i64, y0 as i64);
        let mut value = 0.0;
        for (dy, wy) in [(0i64, 1.0 - fy), (1, fy)] {
            for (dx, wx) in [(0i64, 1.0 - fx), (1, fx)] {
                let col = xi + dx;
                let row = yi + dy;
                if col >= 0
                    && row >= 0
                    && (col as u64) < self.cols as u64
                    && (row as u64) < self.rows as u64
                {
                    value += wx * wy * self.data[row as usize * self.cols as usize + col as usize];
                }
            }
        }
        value
    }

    /// Field value at `query` contributed by a single node, evaluated with
    /// the same cell-center kernel accumulation and bilinear interpolation
    /// the shared raster uses. Sampling the edge's own endpoints through this
    /// matched path and subtracting cancels their contribution without the
    /// raster's discretization bias (an analytic kernel here would not match
    /// what the raster actually stored).
    fn node_sample(&self, query: Vec2, node: Vec2, radius: f32) -> f32 {
        let g = (query - self.origin) / self.cell - Vec2::splat(0.5);
        let x0 = g.x.floor();
        let y0 = g.y.floor();
        let fx = g.x - x0;
        let fy = g.y - y0;
        let (xi, yi) = (x0 as i64, y0 as i64);
        let mut value = 0.0;
        for (dy, wy) in [(0i64, 1.0 - fy), (1, fy)] {
            for (dx, wx) in [(0i64, 1.0 - fx), (1, fx)] {
                let col = xi + dx;
                let row = yi + dy;
                if col < 0
                    || row < 0
                    || (col as u64) >= self.cols as u64
                    || (row as u64) >= self.rows as u64
                {
                    continue;
                }
                let center = Vec2::new(
                    self.origin.x + (col as f32 + 0.5) * self.cell,
                    self.origin.y + (row as f32 + 0.5) * self.cell,
                );
                let base = 1.0 - center.distance(node) / radius;
                if base > 0.0 {
                    value += wx * wy * base * base;
                }
            }
        }
        value
    }

    /// Perpendicular push for an edge's control point, sampled along its chord.
    ///
    /// At each of [`OBSTACLE_SAMPLES`] chord points the field value and its
    /// derivative along the chord normal are read, with the edge's own
    /// endpoints' contributions subtracted through raster-matched single-node
    /// sampling ([`Self::node_sample`]) so an edge never deflects away from
    /// itself — trimming to the node boundary resolves the overlap instead.
    /// The derivative steers toward the sparse side; a small fixed-side kick
    /// handles dense-but-symmetric regions such as an obstacle sitting on the
    /// chord. The total is capped at half the chord length, so a short edge
    /// never bows far beyond its own length.
    pub(crate) fn avoidance_push(
        &self,
        source: Vec2,
        target: Vec2,
        normal: Vec2,
        radius: f32,
        endpoints_in_field: (bool, bool),
    ) -> Vec2 {
        if self.data.is_empty() {
            return Vec2::ZERO;
        }
        let half_len = (target - source).length() * 0.5;
        let step = (radius * OBSTACLE_SIDE_PROBE).max(1e-3);
        let mut scalar = 0.0f32;
        for k in 0..OBSTACLE_SAMPLES {
            let t = k as f32 / (OBSTACLE_SAMPLES - 1) as f32;
            let p = source.lerp(target, t);
            // Densities just off both sides of the chord. When an endpoint is
            // part of the field (see `endpoints_in_field` on
            // `EdgeCurveContext`), its own contribution is cancelled with the
            // raster-matched single-node sampling so an edge never deflects
            // away from itself.
            let mut up = self.sample(p + normal * step);
            let mut down = self.sample(p - normal * step);
            if endpoints_in_field.0 {
                up -= self.node_sample(p + normal * step, source, radius);
                down -= self.node_sample(p - normal * step, source, radius);
            }
            if endpoints_in_field.1 {
                up -= self.node_sample(p + normal * step, target, radius);
                down -= self.node_sample(p - normal * step, target, radius);
            }
            // Steer toward the sparse side: denser below pushes up, and vice
            // versa.
            scalar += OBSTACLE_STEER_GAIN * (down - up);
            // Where mass straddles the chord symmetrically — an obstacle
            // sitting on it — steer toward `-normal`, the default side the
            // previous per-obstacle model used for this case. The steep
            // symmetry gate keeps this kick out of one-sided cases.
            let hi = up.max(down);
            if hi > 1e-6 {
                let symmetry = (up.min(down) / hi).powi(OBSTACLE_KICK_SYMMETRY_POWER);
                scalar -= OBSTACLE_CENTER_KICK * symmetry * hi;
            }
        }
        let push = normal * (scalar * OBSTACLE_FIELD_SCALE * radius);
        if push.length() > half_len {
            push.normalize() * half_len
        } else {
            push
        }
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
#[doc(hidden)]
pub fn signed_densities(midpoints: &[Vec2], normals: &[Vec2], radius: f32) -> Vec<f32> {
    let grid = crate::runtime::DensityGrid::new(midpoints, radius);
    let all: Vec<usize> = (0..midpoints.len()).collect();
    signed_densities_for(&grid, midpoints, normals, radius, &all)
}

/// Compute the signed density for only the edges whose indices are listed.
///
/// The grid is built over every edge's midpoint (so off-screen neighbors still
/// count toward a visible edge's density), but the pairwise loop runs only for
/// the requested indices. This lets the paint layer compute density for just
/// the visible edges after culling, instead of every edge in the graph.
#[doc(hidden)]
pub fn signed_densities_for<S>(
    grid: &crate::runtime::DensityGrid<S>,
    midpoints: &[Vec2],
    normals: &[Vec2],
    radius: f32,
    indices: &[usize],
) -> Vec<f32>
where
    S: BuildHasher + Default + Clone,
{
    let mut result = vec![0.0f32; midpoints.len()];
    for &i in indices {
        let mut signed = 0.0f32;
        for j in grid.candidates(midpoints[i], radius) {
            if i == j {
                continue;
            }
            let delta = midpoints[j] - midpoints[i];
            let Some(dist) = finite_chord_length(midpoints[i], midpoints[j]) else {
                continue;
            };
            if dist > radius {
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
///
/// Obstacle avoidance is deliberately not applied here: [`edge_path`] runs it
/// afterwards, in world space, where both coordinate systems are known (see
/// [`apply_node_avoidance`]).
#[doc(hidden)]
pub fn edge_control_point(
    source: Vec2,
    target: Vec2,
    ctx: &EdgeCurveContext<'_>,
    cluster: Option<(Vec2, f32)>,
) -> Vec2 {
    let dir = target - source;
    let Some(len) = finite_chord_length(source, target) else {
        // No finite non-zero chord can be normalized. Logical self-loops are
        // routed through `self_loop_path` by `edge_path`; a coordinate-only
        // coincidence remains a point for this non-loop helper.
        return if source.is_finite() {
            source
        } else {
            Vec2::ZERO
        };
    };
    let normal = Vec2::new(-dir.y, dir.x) / len;
    let midpoint = (source + target) * 0.5;
    // Parallel fan: separate multiple edges between the same node pair.
    let mut offset = 0.0f32;
    if let Some((position, group_len)) = ctx.parallel[ctx.index] {
        // The spacing scales with the edge's world length: a shorter edge yields
        // a narrower spacing, a longer edge a wider spacing. The power is sub-
        // linear so the sagitta grows more slowly than the chord and curvature
        // still drops for longer edges. The spacing is deliberately zoom-
        // invariant (it depends only on world length, not on the viewport zoom)
        // so the spatial index, the cull test, and the drawn edge all agree at
        // every zoom level.
        let spacing = parallel_spacing(len, 1.0);
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
        // Distance from the center to the chord's midpoint: radius * cos(dθ/2).
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
        return center + outward * control_dist + normal * normal_offset;
    }
    // Density bow: bow toward the side with fewer neighbor edges. The bow is a
    // fraction of edge length, so the curve shape is stable under zoom. When the
    // signed density is zero (no neighbors, or balanced left/right), the bow is
    // zero and the edge is straight.
    let direction = if ctx.signed_density > 0.0 { -1.0 } else { 1.0 };
    let magnitude = (ctx.signed_density.abs() * BOW_DENSITY).min(BOW_MAX);
    let bow = direction * magnitude * len;
    midpoint + normal * (offset + bow)
}

/// Push `control` away from node-dense regions along the chord, so the edge
/// does not run through another node's disc.
///
/// Operates entirely in world space: `control`, `source`, and `target` are
/// world coordinates and the field is built over world positions on a global
/// lattice ([`ObstacleField`]). Curve geometry is therefore a function of the
/// world layout alone — neither panning nor zooming changes a shape. Nodes
/// are world-sized and the clearance ([`EdgeCurveContext::obstacle_radius`])
/// is a plain world length, so both camera operations act purely as
/// transforms over identical geometry.
///
/// Reads the field at a fixed number of chord samples: the density
/// difference across the chord's two sides steers toward the sparse side,
/// and a symmetry-gated kick handles mass straddling the chord, defaulting
/// to `-normal`. The total push is capped at half the chord length so a
/// short edge never bows far beyond its own length. When an edge's endpoints
/// are part of the field (see [`EdgeCurveContext::endpoints_in_field`]) their
/// raster-matched contributions cancel, so an edge never deflects away from
/// itself; trimming to the node boundary resolves the overlap instead.
#[doc(hidden)]
pub fn apply_node_avoidance(
    control: &mut Vec2,
    source: Vec2,
    target: Vec2,
    ctx: &EdgeCurveContext<'_>,
) {
    let Some(len) = finite_chord_length(source, target) else {
        return;
    };
    let dir = target - source;
    let normal = Vec2::new(-dir.y, dir.x) / len;
    let push = ctx.obstacles.avoidance_push(
        source,
        target,
        normal,
        ctx.obstacle_radius,
        ctx.endpoints_in_field,
    );
    *control += push;
}

/// Per-edge geometry context shared by the paint layer and hit testing so the
/// drawn and selectable curves always match.
#[doc(hidden)]
pub struct EdgeCurveContext<'a> {
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
    /// A rasterized obstacle repulsion field over node positions the edge
    /// should bow around, in world coordinates on a globally anchored lattice
    /// (see [`ObstacleField`]).
    pub obstacles: &'a ObstacleField,
    /// World-space obstacle clearance radius: two world node radii plus the
    /// base clearance, matching world-sized nodes.
    pub obstacle_radius: f32,
    /// Whether this edge's source and target nodes are part of the obstacle
    /// field, in `(source, target)` order. Avoidance cancels an endpoint's own
    /// field contribution only when it is actually present: a curve-visible
    /// edge with an off-screen endpoint reads a field built without it, and
    /// cancelling a phantom contribution would deflect the edge wrongly.
    pub endpoints_in_field: (bool, bool),
}

/// The cluster center and radius shared by two nodes, if both belong to the
/// same cluster.
///
/// Returns `Some((center, radius))` only when both endpoints resolve to the
/// same cluster center, so edges within a cluster bow outward from it while
/// edges between clusters (which have different or no centers) keep their
/// normal behavior.
pub fn shared_cluster_center(
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

/// The world-space bounding box of an edge's curve, covering the source, the
/// target, and the control point.
///
/// The returned box covers both capped density-bow directions, the parallel
/// fan/cluster geometry, and the bounded obstacle displacement. The spatial
/// index and the pre-density cull use this conservative owner-local bound so
/// they cannot drop a later exact curve whose endpoints are off-screen.
/// A zero-length chord is returned as a point; graph identity decides whether
/// the edge is a true self-loop and therefore uses viewport-dependent onigiri
/// geometry.
pub fn edge_curve_bbox(
    source: Vec2,
    target: Vec2,
    index: usize,
    has_reverse: &[bool],
    parallel: &[Option<(usize, usize)>],
    cluster: Option<(Vec2, f32)>,
    obstacles: &ObstacleField,
) -> (Vec2, Vec2) {
    // A zero-length chord is only a degenerate non-loop here: callers use the
    // graph edge identity to route true self-loops through self_loop_path. Do
    // not manufacture a loop-like control point from coincident coordinates,
    // because that would make the persistent non-loop index claim coverage for
    // geometry the edge does not paint.
    if finite_chord_length(source, target).is_none() {
        return (source.min(target), source.max(target));
    }

    // The persistent index is built before the visible subset's exact density
    // and obstacle context is known. Bound both density directions at the
    // owning geometry boundary instead of indexing only the zero-density
    // control point. MAX_SIGNED_DENSITY reaches the BOW_MAX cap enforced by
    // edge_control_point after it multiplies by BOW_DENSITY.
    let controls = [
        EdgeCurveContext {
            index,
            signed_density: -MAX_SIGNED_DENSITY,
            has_reverse,
            parallel,
            obstacles,
            obstacle_radius: 0.0,
            // The bbox bound is push-magnitude based (half the chord), so the
            // membership flags cannot change it; an empty membership is the
            // honest default for this bound-only context.
            endpoints_in_field: (false, false),
        },
        EdgeCurveContext {
            index,
            signed_density: MAX_SIGNED_DENSITY,
            has_reverse,
            parallel,
            obstacles,
            obstacle_radius: 0.0,
            // The bbox bound is push-magnitude based (half the chord), so the
            // membership flags cannot change it; an empty membership is the
            // honest default for this bound-only context.
            endpoints_in_field: (false, false),
        },
    ]
    .map(|ctx| edge_control_point(source, target, &ctx, cluster));
    let mut min = source.min(target);
    let mut max = source.max(target);
    for control in controls {
        min = min.min(control);
        max = max.max(control);
    }

    // Obstacle avoidance is bounded by half the chord length in
    // apply_node_avoidance. Expand the world-space box by that bound so the
    // cull and the revision-scoped index remain supersets of the final path,
    // while still keeping the bound proportional to this edge.
    let obstacle_push = (target - source).length() * 0.5;
    let expansion = Vec2::splat(obstacle_push);
    (min - expansion, max + expansion)
}

/// Build the trimmed quadratic Bézier path for an edge, in screen/canvas-local
/// coordinates.
///
/// A self-loop returns the onigiri path; any other edge returns a single
/// segment trimmed to the node boundaries. Both the paint layer and hit testing
/// use this so the drawn and selectable geometry always match.
#[doc(hidden)]
pub fn edge_path<N, E>(
    edge: &Edge<E>,
    ctx: &EdgeCurveContext<'_>,
    graph: &Graph<N, E>,
    node_position: &dyn Fn(NodeId) -> Option<Vec2>,
    node_cluster_center: &dyn Fn(NodeId) -> Option<(Vec2, f32)>,
    viewport: &Viewport,
    style: &GraphStyle,
) -> Vec<Bezier> {
    let source_world = node_position(edge.source).expect("edge source has a position");
    let target_world = node_position(edge.target).expect("edge target has a position");
    let source = viewport.world_to_screen(source_world);
    let target = viewport.world_to_screen(target_world);
    if !source.is_finite() || !target.is_finite() {
        return Vec::new();
    }
    if edge.source == edge.target {
        self_loop_path(edge.source, source, graph, node_position, viewport, style)
    } else if finite_chord_length(source, target).is_none() {
        // Distinct nodes may temporarily occupy the same position. They are
        // not a self-loop, and there is no drawable non-loop segment until
        // their positions diverge.
        Vec::new()
    } else if style.edge_min_length > 0.0
        && finite_chord_length(source, target).is_some_and(|len| len <= style.edge_min_length)
    {
        // Edge-omission LOD: a non-self-loop edge this short on screen is a
        // sub-pixel dot between two tiny nodes, so omit it entirely (no stroke,
        // no arrow). Returning an empty path keeps the drawn geometry, hit
        // testing, and label masking in agreement, and the paint layer already
        // skips empty paths.
        Vec::new()
    } else if straight_edge_applies(source, target, style) {
        // Level-of-detail simplification: when the on-screen chord is short
        // enough that curvature and obstacle bow are visually indistinguishable
        // from a straight line, skip the density/cluster/obstacle control-point
        // work entirely and render a straight segment (a degenerate quadratic
        // with its control point at the chord midpoint) trimmed to the node
        // boundaries. Self-loops are handled above and never simplified. The
        // degenerate control point keeps the trimmed path a valid quadratic so
        // hit testing and label masking keep working unchanged.
        let curve = straight_line_trim(source, target, style.node_screen_radius(viewport.zoom()));
        if finite_chord_length(curve.0, curve.2).is_some() {
            vec![curve]
        } else {
            Vec::new()
        }
    } else {
        // The control point and the node centers share screen coordinates here;
        // only the parallel fan uses a world-length normalization below.
        let cluster_center = shared_cluster_center(edge.source, edge.target, node_cluster_center)
            .map(|(center, radius)| (viewport.world_to_screen(center), radius * viewport.zoom()));
        let mut control = edge_control_point(source, target, ctx, cluster_center);
        if cluster_center.is_none()
            && let Some((position, group_len)) = ctx.parallel[ctx.index]
            && let Some(len) = finite_chord_length(source, target)
        {
            let actual_spacing = parallel_spacing(len, 1.0);
            let desired_spacing = parallel_spacing(len, viewport.zoom());
            let unit = (target - source) / len;
            let normal = Vec2::new(-unit.y, unit.x);
            let offset = (position as f32 - (group_len as f32 - 1.0) * 0.5)
                * (desired_spacing - actual_spacing);
            control += normal * offset;
        }
        // Obstacle avoidance runs in world space so curve shapes depend on the
        // world layout and the zoom level alone, never on where the viewport
        // sits (see `apply_node_avoidance`). With no obstacles the control
        // point is left bit-for-bit untouched: the world roundtrip would
        // otherwise inject float noise into extreme deep-zoom coordinates.
        if !ctx.obstacles.is_empty() {
            let mut control_world = viewport.screen_to_world(control);
            apply_node_avoidance(&mut control_world, source_world, target_world, ctx);
            control = viewport.world_to_screen(control_world);
        }
        #[cfg(test)]
        eprintln!("DBG av post_screen={:?} zoom={}", control, viewport.zoom());
        let curve = trim_curve_to_node_boundary(
            source,
            control,
            target,
            style.node_screen_radius(viewport.zoom()),
        );
        // When the nodes overlap, the trimmed curve is degenerate (its start
        // parameter is not before its end), collapsing to a point. Return an
        // empty path so the edge is skipped rather than producing a zero-length
        // segment whose arrow would normalize to NaN.
        if finite_chord_length(curve.0, curve.2).is_some() {
            vec![curve]
        } else {
            Vec::new()
        }
    }
}

/// Whether a non-self-loop edge between `source` and `target` (screen/canvas-local
/// positions) should be rendered as a straight line under straight-line LOD.
///
/// This mirrors the branch decision in [`edge_path`] so the paint layer can skip
/// density computation for edges that will not bow. The threshold is a screen
/// length; `0.0` disables the simplification. Self-loops are the caller's
/// responsibility and are never routed through here.
#[doc(hidden)]
pub fn straight_edge_applies(source: Vec2, target: Vec2, style: &GraphStyle) -> bool {
    style.edge_straight_threshold > 0.0
        && finite_chord_length(source, target)
            .is_some_and(|len| len <= style.edge_straight_threshold)
}

/// Whether a directed, non-self-loop edge should omit its arrowhead under arrow
/// LOD: when `style.edge_arrow_min_length` is nonzero and the on-screen chord is
/// at or below it.
///
/// A self-loop's arrow is never omitted (`is_self_loop == true` returns `false`)
/// because it is the only direction cue and the loop has no short-chord case. An
/// undirected edge's arrow is handled by the caller (`PaintEdge::omit_arrow` is
/// only consulted for directed edges); this helper is topology-agnostic apart
/// from the self-loop guard.
#[doc(hidden)]
pub fn edge_arrow_omitted(
    source: Vec2,
    target: Vec2,
    is_self_loop: bool,
    style: &GraphStyle,
) -> bool {
    !is_self_loop
        && style.edge_arrow_min_length > 0.0
        && finite_chord_length(source, target).is_some_and(|len| len <= style.edge_arrow_min_length)
}

/// Trim a quadratic Bézier edge `(source, control, target)` along its own path
/// so the endpoints stop just outside each node boundary, emerging from the
/// node center rather than the node edge.
///
/// `source` and `target` are the two node centers. The curve is trimmed at the
/// parameter `t` where it first leaves the source node's boundary and the
/// parameter where it last enters the target node's boundary, found by binary
/// search on the curve parameter so the result is smooth under zoom.
#[doc(hidden)]
pub fn trim_curve_to_node_boundary(
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

/// Trim a straight-line LOD edge from `source` to `target` so its endpoints
/// stop just outside each node boundary, exactly like
/// [`trim_curve_to_node_boundary`] with a control point at the chord midpoint.
///
/// A straight edge's control point equals the chord midpoint, so the "curve" is
/// a line and the parameter where it leaves the source boundary and enters the
/// target boundary can be solved analytically from a line-circle intersection
/// instead of binary search. This is a pure geometry optimization: the returned
/// degenerate quadratic is identical (up to float rounding) to the curve path,
/// so hit testing, label masking, and rendering behave unchanged.
#[doc(hidden)]
fn straight_line_trim(source: Vec2, target: Vec2, radius: f32) -> Bezier {
    let gap = radius + 2.0;
    let t_start = line_boundary_t(source, target, source, gap, true);
    let t_end = line_boundary_t(source, target, target, gap, false);
    sub_bezier(source, (source + target) * 0.5, target, t_start, t_end)
}

/// Find the parameter `t` at which the straight segment from `p0` to `p2` first
/// leaves (`leaving == true`) or last enters (`false`) the node boundary circle
/// centered at `center` with radius `gap`.
///
/// The segment point is `P(t) = p0 + (p2 - p0)·t`. `|P(t) - center| = gap`
/// expands to a quadratic in `t` whose two roots give the entry and exit
/// parameters along the infinite line. For the source end (`center = p0`) the
/// segment leaves the circle at the larger root; for the target end
/// (`center = p2`) it enters at the smaller root. The result is clipped to
/// `[0, 1]`, matching the bounds the binary search returns.
fn line_boundary_t(p0: Vec2, p2: Vec2, center: Vec2, gap: f32, leaving: bool) -> f32 {
    let d = p2 - p0;
    let len_sq = d.length_squared();
    // A zero-length chord cannot leave any circle; both endpoints coincide with
    // the (single) node center, so the trimmed segment is empty. The paint layer
    // rejects such degenerate paths anyway.
    if len_sq == 0.0 {
        return if leaving { 0.0 } else { 1.0 };
    }
    // |P(t) - center|^2 = gap^2 => a·t² + b·t + c = 0 with:
    //   a = d·d
    //   b = 2·d·(p0 - center)
    //   c = |p0 - center|² - gap²
    let oc = p0 - center;
    let a = len_sq;
    let b = 2.0 * d.dot(oc);
    let c = oc.length_squared() - gap * gap;
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        // The chord does not reach `gap` from the center (nodes overlap); the
        // binary search would converge to the degenerate endpoints.
        return if leaving { 0.0 } else { 1.0 };
    }
    let sqrt_disc = disc.sqrt();
    let t_lo = (-b - sqrt_disc) / (2.0 * a);
    let t_hi = (-b + sqrt_disc) / (2.0 * a);
    if leaving {
        t_hi.clamp(0.0, 1.0)
    } else {
        t_lo.clamp(0.0, 1.0)
    }
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
    let gap_sq = gap * gap;
    // 20 iterations give a parameter precision of ~1e-6, far below a pixel at
    // any zoom, so the result stays smooth while halving the per-edge cost.
    for _ in 0..20 {
        let mid = (lo + hi) * 0.5;
        let p = bezier_point(p0, p1, p2, mid);
        // Compare squared distances to avoid a sqrt per iteration. Both sides
        // are non-negative, so this is equivalent to `length() >= gap`.
        let outside = (p - center).length_squared() >= gap_sq;
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
    let r = style.node_screen_radius(viewport.zoom());
    // Two points just outside the node's circumference, symmetric about the
    // up-axis, angled 30° from the up-axis so they are distinct and point at
    // the center. The small outward offset keeps the loop clear of the node.
    let r_out = r + 2.0 * viewport.zoom();
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
            if finite_chord_length(node_pos, other_screen).is_some() {
                sum += delta.normalize();
                count += 1;
            }
        }
        if count > 0 {
            let avg = sum / count as f32;
            if avg.is_finite() && avg.length_squared() > 0.0 {
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

pub(crate) fn point_in_bounds(p: Vec2, bounds: &crate::viewport::WorldBounds, margin: f32) -> bool {
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
    use crate::layout::FixedLayout;
    use crate::patch::GraphBatch;
    use crate::scene::GraphScene;
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
        obstacles: &[Vec2],
        parallel: &'a [Option<(usize, usize)>],
    ) -> EdgeCurveContext<'a> {
        // Foreign obstacles only; the edge's own endpoints are not part of the
        // field, as in the production path for an off-screen endpoint.
        ctx_in_field(index, signed_density, obstacles, parallel, (false, false))
    }

    /// Like [`ctx`], but with explicit endpoint membership — the production
    /// contract when the field is built over nodes that include this edge's
    /// own endpoints.
    fn ctx_in_field<'a>(
        index: usize,
        signed_density: f32,
        obstacles: &[Vec2],
        parallel: &'a [Option<(usize, usize)>],
        endpoints_in_field: (bool, bool),
    ) -> EdgeCurveContext<'a> {
        let grid = Box::leak(Box::new(ObstacleField::new(obstacles, 42.0)));
        EdgeCurveContext {
            index,
            signed_density,
            has_reverse: &[false],
            parallel,
            obstacles: grid,
            obstacle_radius: 42.0,
            endpoints_in_field,
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
            node_overlay: None,
            edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
        });

        assert_eq!(frame.edges.len(), 1);
    }

    #[test]
    fn culled_self_loop_beyond_margin_has_no_on_screen_part() {
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        scene.merge(GraphBatch::new().node("node".to_owned(), ()).edge(
            "loop".to_owned(),
            "node".to_owned(),
            "node".to_owned(),
            EdgeDirection::Directed,
            (),
        ));
        let node = scene.node_id(&"node".to_owned()).unwrap();
        let loop_id = scene.edge_id(&"loop".to_owned()).unwrap();
        scene.set_position(node, Vec2::new(0.0, 2_230.0));
        let graph = scene.graph();
        let positions = |id: NodeId| scene.node_position(id);
        let no_cluster = no_clusters();
        let style = GraphStyle::default();
        let mut viewport = Viewport::new();
        viewport.set_size(Vec2::new(100.0, 100.0));
        viewport.fit_bounds(
            WorldBounds {
                min: Vec2::new(-2_000.0, -2_000.0),
                max: Vec2::new(2_000.0, 2_000.0),
            },
            0.0,
        );
        assert!((viewport.zoom() - 0.025).abs() < 1e-6);
        let visible_world = viewport.visible_world_bounds();
        assert!(
            positions(node).unwrap().y > visible_world.max.y + style.node_radius * 2.0 + 200.0,
            "the loop node must be beyond the ordinary query slack"
        );

        let screen_bounds = WorldBounds {
            min: Vec2::ZERO,
            max: viewport.size(),
        };
        let path = self_loop_path(
            node,
            viewport.world_to_screen(positions(node).unwrap()),
            graph,
            &positions,
            &viewport,
            &style,
        );
        // At this zoom the world-sized loop collapses below a pixel, so the
        // path may legitimately be empty (nothing drawable). When points do
        // exist they must hug the node's screen position.
        let node_screen = viewport.world_to_screen(positions(node).unwrap());
        // Hug bound: effective (floored) node radius plus the loop's
        // base/clearance paddings.
        let r_eff = style.node_screen_radius(viewport.zoom());
        let hug = r_eff + 12.0 * viewport.zoom() + 2.0;
        let hug_bounds = WorldBounds {
            min: node_screen - Vec2::splat(hug),
            max: node_screen + Vec2::splat(hug),
        };
        for (p0, p1, p2) in &path {
            for p in [p0, p1, p2] {
                assert!(
                    point_in_bounds(*p, &hug_bounds, 0.0),
                    "self-loop point {p:?} must hug the node at {node_screen:?}"
                );
            }
        }
        if !path.is_empty() {
            let (path_min, path_max) = path.iter().fold(
                (Vec2::splat(f32::INFINITY), Vec2::splat(f32::NEG_INFINITY)),
                |(min, max), (p0, p1, p2)| {
                    (
                        min.min(*p0).min(*p1).min(*p2),
                        max.max(*p0).max(*p1).max(*p2),
                    )
                },
            );
            assert!(
                !bounds_intersect(&screen_bounds, 0.0, path_min, path_max),
                "the shrunken self-loop must stay outside the viewport"
            );
        }

        // With world-sized nodes the cull margin (two node radii) strictly
        // exceeds anything a self-loop can draw beyond its own node, so a
        // loop whose node lies beyond that margin has no on-screen part and
        // must be culled consistently by every path.
        let linear = build_paint_frame(PaintFrameInput {
            graph,
            node_position: &positions,
            node_cluster_center: &no_cluster,
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &viewport,
            style: &style,
            selection: &Selection::new(),
            hover: &Hover::default(),
            node_overlay: None,
            edge_overlay: None,
        });
        let mut runtime = GraphRuntime::new();
        let synced = scene.sync_runtime(&mut runtime);
        let candidate_ids = synced
            .visible_edge_candidates(&visible_world, style.node_radius * 2.0)
            .into_iter()
            .map(|index| synced.edges().edge_ids[index])
            .collect::<Vec<_>>();
        // Overflow-classified self-loops are always candidates by design;
        // the precise per-frame path decides visibility.
        assert_eq!(candidate_ids, vec![loop_id]);
        let indexed = build_indexed_paint_frame(IndexedPaintFrameInput {
            synced: &synced,
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &viewport,
            style: &style,
            selection: &Selection::new(),
            hover: &Hover::default(),
            node_overlay: None,
            edge_overlay: None,
        });
        let linear_ids = linear.edges.iter().map(|edge| edge.id).collect::<Vec<_>>();
        let indexed_ids = indexed.edges.iter().map(|edge| edge.id).collect::<Vec<_>>();
        assert_eq!(linear_ids, Vec::<EdgeId>::new());
        assert_eq!(indexed_ids, Vec::<EdgeId>::new());
        assert_eq!(indexed_ids, linear_ids);
    }

    #[test]
    fn resync_after_position_mutation_paints_exact_edge_without_stale_runtime() {
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        scene.merge(
            GraphBatch::new()
                .node("source".to_owned(), ())
                .node("target".to_owned(), ())
                .edge(
                    "edge".to_owned(),
                    "source".to_owned(),
                    "target".to_owned(),
                    EdgeDirection::Directed,
                    (),
                ),
        );
        let source = scene.node_id(&"source".to_owned()).unwrap();
        let target = scene.node_id(&"target".to_owned()).unwrap();
        let expected = scene.edge_id(&"edge".to_owned()).unwrap();
        scene.set_position(source, Vec2::new(0.0, 0.0));
        scene.set_position(target, Vec2::new(100.0, 0.0));

        let mut runtime = GraphRuntime::new();
        {
            let _synced = scene.sync_runtime(&mut runtime);
        }
        scene.set_position(source, Vec2::new(1_000.0, 1_000.0));
        scene.set_position(target, Vec2::new(1_100.0, 1_000.0));

        let mut viewport = Viewport::new();
        viewport.set_size(Vec2::new(200.0, 200.0));
        viewport.fit_bounds(
            WorldBounds {
                min: Vec2::new(1_000.0, 1_000.0),
                max: Vec2::new(1_100.0, 1_000.0 + 100.0),
            },
            0.0,
        );
        let style = GraphStyle::default();
        let selection = Selection::new();
        let hover = Hover::default();
        let synced = scene.sync_runtime(&mut runtime);
        let frame = build_indexed_paint_frame(IndexedPaintFrameInput {
            synced: &synced,
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &viewport,
            style: &style,
            selection: &selection,
            hover: &hover,
            node_overlay: None,
            edge_overlay: None,
        });
        assert_eq!(
            frame.edges.iter().map(|edge| edge.id).collect::<Vec<_>>(),
            vec![expected]
        );
    }

    #[test]
    fn resync_after_topology_mutation_paints_exact_new_edge_without_stale_indices() {
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        scene.merge(GraphBatch::new().node("a".to_owned(), ()));
        let a = scene.node_id(&"a".to_owned()).unwrap();
        scene.set_position(a, Vec2::new(0.0, 0.0));
        let mut runtime = GraphRuntime::new();
        {
            let _synced = scene.sync_runtime(&mut runtime);
        }

        scene.merge(GraphBatch::new().node("b".to_owned(), ()).edge(
            "ab".to_owned(),
            "a".to_owned(),
            "b".to_owned(),
            EdgeDirection::Directed,
            (),
        ));
        let b = scene.node_id(&"b".to_owned()).unwrap();
        let expected = scene.edge_id(&"ab".to_owned()).unwrap();
        scene.set_position(b, Vec2::new(100.0, 0.0));
        let mut viewport = Viewport::new();
        viewport.set_size(Vec2::new(200.0, 200.0));
        viewport.fit_bounds(
            WorldBounds {
                min: Vec2::new(-10.0, -10.0),
                max: Vec2::new(110.0, 10.0),
            },
            0.0,
        );
        let style = GraphStyle::default();
        let synced = scene.sync_runtime(&mut runtime);
        let frame = build_indexed_paint_frame(IndexedPaintFrameInput {
            synced: &synced,
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &viewport,
            style: &style,
            selection: &Selection::new(),
            hover: &Hover::default(),
            node_overlay: None,
            edge_overlay: None,
        });
        assert_eq!(
            frame.edges.iter().map(|edge| edge.id).collect::<Vec<_>>(),
            vec![expected]
        );
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
            node_overlay: None,
            edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
        });

        let painted = frame.nodes.iter().find(|n| n.id == node).unwrap();
        assert!(painted.selected);
        assert!(painted.hovered);
    }

    #[test]
    fn overlay_is_independent_of_selection_and_hover() {
        let mut g: Graph<(), ()> = Graph::new();
        let accent = g.add_node(());
        let dimmed = g.add_node(());
        let plain = g.add_node(());
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(100.0, 100.0));
        let style = GraphStyle::default();

        // `accent` is both selected and overlaid Accent; `dimmed` is overlaid
        // Dimmed; `plain` has no overlay. The overlay categories must be
        // readable independently of selection state.
        let mut selection = Selection::new();
        selection.nodes.push(accent);
        let node_overlay = move |id: NodeId| {
            if id == accent {
                OverlayCategory::Accent
            } else if id == dimmed {
                OverlayCategory::Dimmed
            } else {
                OverlayCategory::None
            }
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
            hover: &Hover::default(),
            node_overlay: Some(&node_overlay),
            edge_overlay: None,
        });

        let painted_accent = frame.nodes.iter().find(|n| n.id == accent).unwrap();
        assert_eq!(painted_accent.overlay, OverlayCategory::Accent);
        assert!(painted_accent.selected, "selected and overlay must coexist");
        let painted_dimmed = frame.nodes.iter().find(|n| n.id == dimmed).unwrap();
        assert_eq!(painted_dimmed.overlay, OverlayCategory::Dimmed);
        assert!(!painted_dimmed.selected);
        let painted_plain = frame.nodes.iter().find(|n| n.id == plain).unwrap();
        assert_eq!(painted_plain.overlay, OverlayCategory::None);
        assert!(!painted_plain.selected);
    }

    #[test]
    fn absent_overlay_resolver_keeps_base_style() {
        let g = graph();
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(100.0, 100.0));
        let style = GraphStyle::default();
        // No overlay resolver: every node keeps the base (None) category.
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
            node_overlay: None,
            edge_overlay: None,
        });
        for node in &frame.nodes {
            assert_eq!(node.overlay, OverlayCategory::None);
        }
        for edge in &frame.edges {
            assert_eq!(edge.overlay, OverlayCategory::None);
        }
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
            node_overlay: None,
            edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
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
                node_overlay: None,
                edge_overlay: None,
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
            node_overlay: None,
            edge_overlay: None,
        });

        // Find the horizontal edge a->b by its endpoints.
        let horizontal = frame
            .edges
            .iter()
            .find(|e| (e.source.y - e.target.y).abs() < 1e-3)
            .expect("horizontal edge exists");
        let (_, control, _) = horizontal.path[0];
        // The neighbor edge's midpoint (25, 15) sits above the chord, and so
        // does node c. Both effects push the same way: the density bow favors
        // the sparse lower side, and the obstacle field steers away from the
        // mass above the chord. With y growing downward on screen, both give
        // a negative control offset.
        assert!(
            control.y < 0.0,
            "edge should bow away from the crowded upper side, control = {control:?}"
        );
    }

    #[test]
    fn edge_bow_grows_with_density_difference() {
        // The bow magnitude grows with the signed density difference. A lone
        // edge (density 0) is straight; a neighbor on the left (density +1)
        // bows right.
        let source = Vec2::new(0.0, 0.0);
        let target = Vec2::new(100.0, 0.0);
        let lone = edge_control_point(source, target, &ctx(0, 0.0, &[], &[None]), None);
        let with_neighbor = edge_control_point(source, target, &ctx(0, 1.0, &[], &[None]), None);
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
        // midpoint, normalized by the edge length, for two edge lengths (the
        // same world edge at two zooms maps to two different world lengths).
        let bow_ratio = |source: Vec2, target: Vec2| {
            let control = edge_control_point(source, target, &ctx(0, 0.0, &[], &[None]), None);
            let mid = (source + target) * 0.5;
            (control - mid).length() / (target - source).length()
        };
        // Zoom 1: world a(0,0) b(100,0) maps to screen (0,0) and (100,0).
        let r1 = bow_ratio(Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0));
        // Zoom 2: the same world edge maps to screen (0,0) and (200,0).
        let r2 = bow_ratio(Vec2::new(0.0, 0.0), Vec2::new(200.0, 0.0));
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
            let control =
                edge_control_point(source, target, &ctx(0, 0.0, &[], &[Some((0, 2))]), None);
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
            &ctx(0, 0.0, &[], &[None]),
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
        let straight = edge_control_point(source, target, &ctx(0, 0.0, &[], &[None]), None);
        assert!(
            (straight - mid).length() < 1e-3,
            "unclustered edge should be straight"
        );
    }

    /// Run world-space avoidance for a chord from (0,0) to (100,0) with
    /// `obstacles` in the field, returning the displaced control point.
    fn avoid(obstacles: &[Vec2], flags: (bool, bool)) -> Vec2 {
        let mut control = Vec2::new(50.0, 0.0);
        apply_node_avoidance(
            &mut control,
            Vec2::new(0.0, 0.0),
            Vec2::new(100.0, 0.0),
            &ctx_in_field(0, 0.0, obstacles, &[None], flags),
        );
        control
    }

    #[test]
    fn edge_bows_away_from_obstacle_node() {
        // An obstacle node sitting exactly on the chord must bow the control
        // point off the chord, toward `-normal` — downward here — matching
        // the default side of the per-obstacle model this field replaced.
        let control = avoid(&[Vec2::new(50.0, 0.0)], (false, false));
        assert!(
            control.y < -1e-3,
            "edge should bow below, the default side for an obstacle on the chord (control={control:?})"
        );
    }

    #[test]
    fn avoidance_steers_to_the_sparser_side() {
        // An obstacle above the chord pushes the control point below it, and a
        // mirrored obstacle below pushes above: the curve steers toward the
        // sparse side of its chord.
        let above = avoid(&[Vec2::new(50.0, 12.0)], (false, false));
        assert!(
            above.y < 0.0,
            "obstacle above should push the control point below (control={above:?})"
        );
        let below = avoid(&[Vec2::new(50.0, -12.0)], (false, false));
        assert!(
            below.y > 0.0,
            "obstacle below should push the control point above (control={below:?})"
        );
    }

    #[test]
    fn avoidance_ignores_own_endpoints() {
        // When the field includes this edge's own endpoints their
        // raster-matched contributions cancel exactly, so a chord between two
        // nodes stays straight and trimming to the node boundary handles the
        // overlap instead.
        let control = avoid(&[Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0)], (true, true));
        assert!(
            (control - Vec2::new(50.0, 0.0)).length() < 1e-3,
            "own endpoints must not deflect their edge (control={control:?})"
        );
    }

    #[test]
    fn avoidance_skips_endpoints_absent_from_field() {
        // A curve-visible edge can have an off-screen endpoint, so the field
        // is built without it. The membership flags say so, and no phantom
        // contribution may be cancelled: the chord still bows around a real
        // obstacle on it.
        let control = avoid(&[Vec2::new(50.0, 0.0)], (false, false));
        assert!(
            control.y < -1.0,
            "edge should bow below despite absent endpoints (control={control:?})"
        );
    }

    #[test]
    fn avoidance_ignores_distant_obstacles() {
        // Obstacles clustered far from the chord leave it untouched.
        let control = avoid(&[Vec2::new(10000.0, 10000.0)], (false, false));
        assert!(
            (control - Vec2::new(50.0, 0.0)).length() < 1e-3,
            "distant obstacles must not deflect the edge (control={control:?})"
        );
    }

    #[test]
    fn avoidance_is_pan_invariant() {
        // Panning translates every world position by the same offset and
        // slides the visible window across the raster's global lattice. The
        // push for a given edge configuration must be identical under that
        // translation whatever the lattice phase of the original placement —
        // otherwise curves would wobble as the view moves.
        let shift = Vec2::new(137.31, -91.73);
        let push_at = |offset: Vec2| {
            let source = offset;
            let target = offset + Vec2::new(100.0, 0.0);
            let obstacles = [
                offset + Vec2::new(50.0, 12.0),
                offset + Vec2::new(20.0, -30.0),
            ];
            let mut control = (source + target) * 0.5;
            apply_node_avoidance(
                &mut control,
                source,
                target,
                &ctx_in_field(0, 0.0, &obstacles, &[None], (true, true)),
            );
            control - (source + target) * 0.5
        };
        let base = push_at(Vec2::ZERO);
        let moved = push_at(shift);
        assert!(
            (base - moved).length() < 1e-3,
            "panning must not change the avoidance push (base={base:?}, moved={moved:?})"
        );
    }

    #[test]
    fn obstacle_field_is_deterministic_and_bounded() {
        let points = [Vec2::new(0.0, 0.0), Vec2::new(30.0, 10.0)];
        let a = ObstacleField::new(&points, 42.0);
        let b = ObstacleField::new(&points, 42.0);
        assert_eq!(a.data, b.data, "same input must build the same raster");
        // A pathologically wide extent grows the cell size instead of the
        // allocation.
        let spread_points = [Vec2::new(0.0, 0.0), Vec2::new(1.0e6, 1.0e6)];
        let spread = ObstacleField::new(&spread_points, 42.0);
        assert!(
            (spread.cols as usize) * (spread.rows as usize) <= OBSTACLE_FIELD_MAX_CELLS,
            "raster must stay bounded ({}x{} cells)",
            spread.cols,
            spread.rows
        );
    }

    #[test]
    fn parallel_spacing_varies_with_length_but_not_zoom() {
        // The spacing scales with the edge's world length: a longer edge yields
        // a wider spacing. It is deliberately zoom-invariant (independent of the
        // viewport zoom) so the spatial index, the cull test, and the drawn edge
        // all agree at every zoom level.
        let control = |source: Vec2, target: Vec2| {
            edge_control_point(source, target, &ctx(0, 0.0, &[], &[Some((0, 2))]), None)
        };
        // Base length.
        let base = control(Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0));
        // Longer edge widens the spacing (larger world length).
        let long = control(Vec2::new(0.0, 0.0), Vec2::new(200.0, 0.0));
        assert!(
            long.y.abs() > base.y.abs(),
            "longer edge should widen the spacing (base={}, long={})",
            base.y,
            long.y
        );
        // The helper receives coordinates in the current space. Scaling both
        // the coordinates and the coordinate scale must therefore scale the
        // spacing by exactly the same factor while preserving its world-space
        // value.
        let world_len = 100.0;
        let zoom = 2.0;
        let world_spacing = parallel_spacing(world_len, 1.0);
        let zoomed_spacing = parallel_spacing(world_len * zoom, zoom);
        assert!(
            (zoomed_spacing / zoom - world_spacing).abs() < 1e-4,
            "spacing should preserve world value under coordinate scaling (world={world_spacing}, zoomed={zoomed_spacing})"
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
            node_overlay: None,
            edge_overlay: None,
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

    #[test]
    fn density_bowed_edge_survives_linear_and_indexed_cull() {
        // The long edge is far above the view at both endpoints. Six nearby
        // parallel midpoints on its left produce a capped density bow whose
        // control point reaches well below the zero-density endpoint box.
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        let mut batch = GraphBatch::new()
            .node("source".to_owned(), ())
            .node("target".to_owned(), ())
            .node("near-source".to_owned(), ())
            .node("near-target".to_owned(), ())
            .edge(
                "density-edge".to_owned(),
                "source".to_owned(),
                "target".to_owned(),
                EdgeDirection::Directed,
                (),
            );
        for index in 0..6 {
            batch = batch.edge(
                format!("near-{index}"),
                "near-source".to_owned(),
                "near-target".to_owned(),
                EdgeDirection::Directed,
                (),
            );
        }
        scene.merge(batch);

        let source = scene.node_id(&"source".to_owned()).unwrap();
        let target = scene.node_id(&"target".to_owned()).unwrap();
        let near_source = scene.node_id(&"near-source".to_owned()).unwrap();
        let near_target = scene.node_id(&"near-target".to_owned()).unwrap();
        scene.set_position(source, Vec2::new(-1_000.0, 1_000.0));
        scene.set_position(target, Vec2::new(1_000.0, 1_000.0));
        scene.set_position(near_source, Vec2::new(-10.0, 1_010.0));
        scene.set_position(near_target, Vec2::new(10.0, 1_010.0));

        let mut viewport = Viewport::new();
        viewport.set_size(Vec2::new(200.0, 200.0));
        viewport.fit_bounds(
            WorldBounds {
                min: Vec2::new(-100.0, 0.0),
                max: Vec2::new(100.0, 200.0),
            },
            0.0,
        );
        let visible = viewport.visible_world_bounds();
        assert!(!point_in_bounds(
            scene.node_position(source).unwrap(),
            &visible,
            0.0
        ));
        assert!(!point_in_bounds(
            scene.node_position(target).unwrap(),
            &visible,
            0.0
        ));

        let mut runtime = GraphRuntime::new();
        let synced = scene.sync_runtime(&mut runtime);
        let prep = synced.edges();
        let edge_id = scene.edge_id(&"density-edge".to_owned()).unwrap();
        let edge_index = prep
            .edge_ids
            .iter()
            .position(|id| *id == edge_id)
            .expect("density edge is preprocessed");
        let densities = signed_densities_for(
            &prep.density_grid,
            &prep.midpoints,
            &prep.normals,
            DENSITY_RADIUS,
            &[edge_index],
        );
        assert!(
            densities[edge_index] > 4.0,
            "fixture must create a strong one-sided density: {}",
            densities[edge_index]
        );

        let empty_obstacles = ObstacleField::new(&[], 1.0);
        let actual_control = edge_control_point(
            prep.source[edge_index],
            prep.target[edge_index],
            &EdgeCurveContext {
                index: edge_index,
                signed_density: densities[edge_index],
                has_reverse: &prep.has_reverse,
                parallel: &prep.parallel,
                obstacles: &empty_obstacles,
                obstacle_radius: 0.0,
                endpoints_in_field: (false, false),
            },
            None,
        );
        assert!(
            actual_control.y < -700.0,
            "density bow must leave the zero-density endpoint box: {actual_control:?}"
        );
        let zero_control = edge_control_point(
            prep.source[edge_index],
            prep.target[edge_index],
            &EdgeCurveContext {
                index: edge_index,
                signed_density: 0.0,
                has_reverse: &prep.has_reverse,
                parallel: &prep.parallel,
                obstacles: &empty_obstacles,
                obstacle_radius: 0.0,
                endpoints_in_field: (false, false),
            },
            None,
        );
        assert!(
            !bounds_intersect(
                &visible,
                crate::runtime::EDGE_INDEX_SLACK,
                prep.source[edge_index]
                    .min(prep.target[edge_index])
                    .min(zero_control),
                prep.source[edge_index]
                    .max(prep.target[edge_index])
                    .max(zero_control),
            ),
            "the old zero-density bbox must miss the view even after index slack"
        );

        let candidates = synced.visible_edge_candidates(&visible, 0.0);
        assert!(
            candidates.contains(&edge_index),
            "conservative indexed bbox must retain the density-bowed edge"
        );

        let graph = scene.graph();
        let positions = |id: NodeId| scene.node_position(id);
        let style = GraphStyle::default();
        let selection = Selection::new();
        let hover = Hover::default();
        let linear = build_paint_frame(PaintFrameInput {
            graph,
            node_position: &positions,
            node_cluster_center: &|id| scene.node_cluster_center(id),
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &viewport,
            style: &style,
            selection: &selection,
            hover: &hover,
            node_overlay: None,
            edge_overlay: None,
        });
        let indexed = build_indexed_paint_frame(IndexedPaintFrameInput {
            synced: &synced,
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &viewport,
            style: &style,
            selection: &selection,
            hover: &hover,
            node_overlay: None,
            edge_overlay: None,
        });
        let linear_ids = linear.edges.iter().map(|edge| edge.id).collect::<Vec<_>>();
        let indexed_ids = indexed.edges.iter().map(|edge| edge.id).collect::<Vec<_>>();
        assert_eq!(linear_ids, vec![edge_id]);
        assert_eq!(indexed_ids, vec![edge_id]);
        let screen_bounds = WorldBounds {
            min: Vec2::ZERO,
            max: viewport.size(),
        };
        assert!(
            linear.edges[0].path.iter().any(|(p0, p1, p2)| {
                let midpoint = bezier_point(*p0, *p1, *p2, 0.5);
                point_in_bounds(midpoint, &screen_bounds, 0.0)
            }),
            "an actual midpoint on the painted density-bowed path must enter the viewport"
        );
    }

    #[test]
    fn spatial_index_matches_linear_scan_visible_set() {
        // Build a graph with nodes spread across a wide area and a long edge
        // crossing the view. The spatial index must return the same visible node
        // and edge id sets as the linear scan, for both an overview and a
        // deep-zoom viewport.
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        let mut batch = GraphBatch::new();
        for i in 0..8 {
            batch = batch.node(i.to_string(), ());
        }
        for i in 0..7 {
            batch = batch.edge(
                format!("e{i}"),
                i.to_string(),
                (i + 1).to_string(),
                EdgeDirection::Directed,
                (),
            );
        }
        // A long edge from the first to the last node, crossing the whole graph.
        batch = batch.edge(
            "long".to_owned(),
            "0".to_owned(),
            "7".to_owned(),
            EdgeDirection::Directed,
            (),
        );
        scene.merge(batch);
        let ids = (0..8)
            .map(|i| scene.node_id(&i.to_string()).unwrap())
            .collect::<Vec<_>>();
        for (i, &id) in ids.iter().enumerate() {
            scene.set_position(id, Vec2::new(i as f32 * 200.0, (i % 2) as f32 * 200.0));
        }
        let graph = scene.graph();

        let pos = |id: NodeId| scene.node_position(id);
        let style = GraphStyle::default();
        let selection = Selection::new();
        let hover = Hover::default();

        let run_linear = |vp: &Viewport| {
            let frame = build_paint_frame(PaintFrameInput {
                graph,
                node_position: &pos,
                node_cluster_center: &no_clusters(),
                node_label: &no_labels(),
                edge_label: &no_edge_labels(),
                viewport: vp,
                style: &style,
                selection: &selection,
                hover: &hover,
                node_overlay: None,
                edge_overlay: None,
            });
            let mut nodes: Vec<NodeId> = frame.nodes.iter().map(|n| n.id).collect();
            nodes.sort();
            let mut edges: Vec<EdgeId> = frame.edges.iter().map(|e| e.id).collect();
            edges.sort();
            (nodes, edges)
        };

        // Overview viewport: everything visible.
        let mut overview = Viewport::new();
        overview.set_size(Vec2::new(1600.0, 1000.0));
        overview.fit_bounds(
            WorldBounds {
                min: Vec2::new(0.0, 0.0),
                max: Vec2::new(1400.0, 200.0),
            },
            0.0,
        );

        // Deep-zoom viewport near the origin: only the first few nodes and the
        // long edge crossing the view are visible.
        let mut deep = Viewport::new();
        deep.set_size(Vec2::new(400.0, 400.0));
        deep.fit_bounds(
            WorldBounds {
                min: Vec2::new(-50.0, -50.0),
                max: Vec2::new(250.0, 250.0),
            },
            0.0,
        );

        for vp in [&overview, &deep] {
            let mut rt = GraphRuntime::new();
            let scan = run_linear(vp);
            let synced = scene.sync_runtime(&mut rt);
            let frame = build_indexed_paint_frame(IndexedPaintFrameInput {
                synced: &synced,
                node_label: &no_labels(),
                edge_label: &no_edge_labels(),
                viewport: vp,
                style: &style,
                selection: &selection,
                hover: &hover,
                node_overlay: None,
                edge_overlay: None,
            });
            let mut nodes = frame.nodes.iter().map(|node| node.id).collect::<Vec<_>>();
            nodes.sort();
            let mut edges = frame.edges.iter().map(|edge| edge.id).collect::<Vec<_>>();
            edges.sort();
            let indexed = (nodes, edges);
            assert_eq!(indexed, scan, "indexed visible set must match linear scan");
        }
    }

    #[test]
    fn high_zoom_parallel_fan_index_matches_linear_screen_visibility() {
        // Both endpoints are outside the view. At high zoom, the first parallel
        // edge's world-space fan reaches into the view after the world-to-screen
        // transform, while the old screen-length fan would remain outside.
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        scene.merge(
            GraphBatch::new()
                .node("source".to_owned(), ())
                .node("target".to_owned(), ())
                .edge(
                    "target".to_owned(),
                    "source".to_owned(),
                    "target".to_owned(),
                    EdgeDirection::Directed,
                    (),
                )
                .edge(
                    "other".to_owned(),
                    "source".to_owned(),
                    "target".to_owned(),
                    EdgeDirection::Directed,
                    (),
                ),
        );
        let source = scene.node_id(&"source".to_owned()).unwrap();
        let target = scene.node_id(&"target".to_owned()).unwrap();
        let target_edge = scene.edge_id(&"target".to_owned()).unwrap();
        let other_edge = scene.edge_id(&"other".to_owned()).unwrap();
        scene.set_position(source, Vec2::new(-100.0, 47.5));
        scene.set_position(target, Vec2::new(100.0, 47.5));
        let graph = scene.graph();
        let positions = |id: NodeId| scene.node_position(id);
        let style = GraphStyle::default();
        let mut viewport = Viewport::new();
        viewport.set_size(Vec2::new(100.0, 100.0));
        viewport.fit_bounds(
            WorldBounds {
                min: Vec2::new(-25.0, -25.0),
                max: Vec2::new(25.0, 25.0),
            },
            0.0,
        );
        assert!((viewport.zoom() - 2.0).abs() < 1e-6);
        let visible = viewport.visible_world_bounds();
        assert!(!point_in_bounds(positions(source).unwrap(), &visible, 0.0));
        assert!(!point_in_bounds(positions(target).unwrap(), &visible, 0.0));

        let no_cluster = |id: NodeId| scene.node_cluster_center(id);
        let no_label = no_labels();
        let no_edge_label = no_edge_labels();
        let selection = Selection::new();
        let hover = Hover::default();
        let mut runtime = GraphRuntime::new();
        let synced = scene.sync_runtime(&mut runtime);
        let prep = synced.edges();
        let target_index = prep
            .edge_ids
            .iter()
            .position(|id| *id == target_edge)
            .expect("target edge is in the runtime prep");
        let candidates = synced.visible_edge_candidates(&visible, style.node_radius * 2.0);
        assert!(
            candidates.contains(&target_index),
            "indexed candidate set must include the fanned target edge"
        );

        // The world-space fan offset is half of the spacing for the first edge.
        // With world length 200, spacing is about 64.3, so its control point is
        // at y ~= 15.3, inside the world viewport. The transformed control point
        // is therefore inside the screen viewport as well.
        let world_control = edge_control_point(
            positions(source).unwrap(),
            positions(target).unwrap(),
            &EdgeCurveContext {
                index: target_index,
                signed_density: 0.0,
                has_reverse: &prep.has_reverse,
                parallel: &prep.parallel,
                obstacles: &ObstacleField::new(&[], style.node_radius * 2.0 + OBSTACLE_RADIUS),
                obstacle_radius: style.node_radius * 2.0 + OBSTACLE_RADIUS,
                endpoints_in_field: (false, false),
            },
            None,
        );
        let intended_screen_control = viewport.world_to_screen(world_control);
        assert!(
            intended_screen_control.x >= 0.0
                && intended_screen_control.x <= viewport.size().x
                && intended_screen_control.y >= 0.0
                && intended_screen_control.y <= viewport.size().y,
            "world-space fan should transform into the viewport (world={world_control:?}, screen={intended_screen_control:?})"
        );

        let indexed = build_indexed_paint_frame(IndexedPaintFrameInput {
            synced: &synced,
            node_label: &no_label,
            edge_label: &no_edge_label,
            viewport: &viewport,
            style: &style,
            selection: &selection,
            hover: &hover,
            node_overlay: None,
            edge_overlay: None,
        })
        .edges
        .into_iter()
        .map(|edge| edge.id)
        .collect::<Vec<_>>();
        assert_eq!(indexed, vec![target_edge]);

        // The linear visibility oracle follows the actual screen-space path,
        // rather than the world-space cull box used by the indexed query.
        let empty_obstacles = ObstacleField::new(&[], style.node_radius * 2.0 + OBSTACLE_RADIUS);
        let screen_bounds = WorldBounds {
            min: Vec2::ZERO,
            max: viewport.size(),
        };
        let linear = prep
            .edge_ids
            .iter()
            .enumerate()
            .filter_map(|(index, id)| {
                let edge = graph.edge(*id)?;
                let path = edge_path(
                    edge,
                    &EdgeCurveContext {
                        index,
                        signed_density: 0.0,
                        has_reverse: &prep.has_reverse,
                        parallel: &prep.parallel,
                        obstacles: &empty_obstacles,
                        obstacle_radius: style.node_radius * 2.0 + OBSTACLE_RADIUS,
                        endpoints_in_field: (false, false),
                    },
                    graph,
                    &positions,
                    &no_cluster,
                    &viewport,
                    &style,
                );
                let intersects = path.iter().any(|(p0, p1, p2)| {
                    bounds_intersect(
                        &screen_bounds,
                        0.0,
                        p0.min(*p1).min(*p2),
                        p0.max(*p1).max(*p2),
                    )
                });
                intersects.then_some(*id)
            })
            .collect::<Vec<_>>();

        assert_eq!(linear, vec![target_edge]);
        assert_eq!(
            indexed, linear,
            "indexed visible edge set must match linear screen visibility"
        );

        // Before the fix, parallel spacing was based on the current screen
        // length. At zoom 2 that fan offset is only about 34.5 pixels, leaving
        // the old control point at y ~= 110.5, outside the 100-pixel viewport.
        let source_screen = viewport.world_to_screen(positions(source).unwrap());
        let target_screen = viewport.world_to_screen(positions(target).unwrap());
        let screen_dir = target_screen - source_screen;
        let screen_normal = Vec2::new(-screen_dir.y, screen_dir.x).normalize();
        let legacy_control = (source_screen + target_screen) * 0.5
            + screen_normal * (-0.5 * parallel_spacing(screen_dir.length(), 1.0));
        let legacy_curve = trim_curve_to_node_boundary(
            source_screen,
            legacy_control,
            target_screen,
            style.node_radius,
        );
        assert!(
            !bounds_intersect(
                &screen_bounds,
                0.0,
                legacy_curve.0.min(legacy_curve.1).min(legacy_curve.2),
                legacy_curve.0.max(legacy_curve.1).max(legacy_curve.2),
            ),
            "pre-fix screen-length fan should miss the viewport (control={legacy_control:?})"
        );
        assert_ne!(target_edge, other_edge);
    }

    #[test]
    fn near_degenerate_parallel_edge_bbox_matches_screen_path() {
        // A finite world chord below f32::EPSILON becomes a visible screen
        // chord at deep zoom. The outer edge of this 200-edge fan therefore
        // exercises the world bbox and the transformed draw path at different
        // coordinate scales without using topology self-loop semantics.
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        let mut batch = GraphBatch::new()
            .node("source".to_owned(), ())
            .node("target".to_owned(), ());
        for index in 0..200 {
            batch = batch.edge(
                format!("fan-{index}"),
                "source".to_owned(),
                "target".to_owned(),
                EdgeDirection::Directed,
                (),
            );
        }
        scene.merge(batch);

        let source = scene.node_id(&"source".to_owned()).unwrap();
        let target = scene.node_id(&"target".to_owned()).unwrap();
        let target_edge = scene.edge_id(&"fan-199".to_owned()).unwrap();
        scene.set_position(source, Vec2::new(0.0, 0.0));
        scene.set_position(target, Vec2::new(1.0e-8, 0.0));

        let graph = scene.graph();
        let positions = |id: NodeId| scene.node_position(id);
        let no_cluster = |id: NodeId| scene.node_cluster_center(id);
        let style = GraphStyle::default().with_node_radius(0.0);
        let mut runtime = GraphRuntime::new();
        let synced = scene.sync_runtime(&mut runtime);
        let prep = synced.edges();
        let target_index = prep
            .edge_ids
            .iter()
            .position(|id| *id == target_edge)
            .expect("outer fan edge is preprocessed");
        let source_world = prep.source[target_index];
        let target_world = prep.target[target_index];
        assert!((target_world - source_world).length() < f32::EPSILON);
        assert!(finite_chord_length(source_world, target_world).is_some());
        assert_ne!(
            source, target,
            "the fixture must use distinct node identities"
        );

        let empty_obstacles = ObstacleField::new(&[], OBSTACLE_RADIUS);
        let world_control = edge_control_point(
            source_world,
            target_world,
            &EdgeCurveContext {
                index: target_index,
                signed_density: 0.0,
                has_reverse: &prep.has_reverse,
                parallel: &prep.parallel,
                obstacles: &empty_obstacles,
                obstacle_radius: 0.0,
                endpoints_in_field: (false, false),
            },
            None,
        );
        assert!(
            world_control.y > 500.0,
            "outer fan control must retain its finite near-degenerate offset: {world_control:?}"
        );

        // Center the deep-zoom viewport on the quadratic midpoint. The old
        // point bbox plus index slack remains hundreds of world units away,
        // while the actual curve midpoint is inside the screen.
        let view_center = Vec2::new(
            (source_world.x + target_world.x) * 0.5,
            world_control.y * 0.5,
        );
        let half_extent = Vec2::splat(0.00005);
        let mut viewport = Viewport::new();
        viewport.set_size(Vec2::splat(100.0));
        viewport.fit_bounds(
            WorldBounds {
                min: -half_extent,
                max: half_extent,
            },
            0.0,
        );
        viewport.focus(view_center);
        assert!((viewport.zoom() - 1.0e6).abs() < 1.0);
        let visible = viewport.visible_world_bounds();
        assert!(!point_in_bounds(source_world, &visible, 0.0));
        assert!(!point_in_bounds(target_world, &visible, 0.0));
        assert!(
            !bounds_intersect(
                &visible,
                crate::runtime::EDGE_INDEX_SLACK,
                source_world.min(target_world),
                source_world.max(target_world),
            ),
            "the old point bbox plus index slack must miss the deep-zoom view"
        );

        let candidates = synced.visible_edge_candidates(&visible, 0.0);
        assert!(
            candidates.contains(&target_index),
            "the finite near-degenerate curve bbox must retain the target candidate"
        );

        let target_record = graph.edge(target_edge).expect("target edge exists");
        let screen_path = edge_path(
            target_record,
            &EdgeCurveContext {
                index: target_index,
                signed_density: 0.0,
                has_reverse: &prep.has_reverse,
                parallel: &prep.parallel,
                obstacles: &empty_obstacles,
                obstacle_radius: style.node_radius * 2.0 + OBSTACLE_RADIUS,
                endpoints_in_field: (false, false),
            },
            graph,
            &positions,
            &no_cluster,
            &viewport,
            &style,
        );
        let screen_bounds = WorldBounds {
            min: Vec2::ZERO,
            max: viewport.size(),
        };
        let enters_screen = screen_path.iter().any(|(p0, p1, p2)| {
            if p0.y > screen_bounds.max.y || p1.y < screen_bounds.min.y {
                return false;
            }
            let mut lo = 0.0;
            let mut hi = 0.5;
            for _ in 0..32 {
                let t = (lo + hi) * 0.5;
                if bezier_point(*p0, *p1, *p2, t).y < screen_bounds.min.y {
                    lo = t;
                } else {
                    hi = t;
                }
            }
            point_in_bounds(
                bezier_point(*p0, *p1, *p2, (lo + hi) * 0.5),
                &screen_bounds,
                0.0,
            )
        });
        assert!(
            enters_screen,
            "the actual transformed curve must enter the viewport: control={world_control:?}, center={view_center:?}, path={screen_path:?}"
        );

        let selection = Selection::new();
        let hover = Hover::default();
        let linear = build_paint_frame(PaintFrameInput {
            graph,
            node_position: &positions,
            node_cluster_center: &no_cluster,
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &viewport,
            style: &style,
            selection: &selection,
            hover: &hover,
            node_overlay: None,
            edge_overlay: None,
        });
        let indexed = build_indexed_paint_frame(IndexedPaintFrameInput {
            synced: &synced,
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &viewport,
            style: &style,
            selection: &selection,
            hover: &hover,
            node_overlay: None,
            edge_overlay: None,
        });
        let mut linear_ids = linear.edges.iter().map(|edge| edge.id).collect::<Vec<_>>();
        let mut indexed_ids = indexed.edges.iter().map(|edge| edge.id).collect::<Vec<_>>();
        linear_ids.sort();
        indexed_ids.sort();
        assert!(
            linear_ids.contains(&target_edge),
            "linear rendering must preserve the exact outer fan edge ID"
        );
        assert!(
            indexed_ids.contains(&target_edge),
            "indexed rendering must preserve the exact outer fan edge ID"
        );
        assert_eq!(indexed_ids, linear_ids);
    }

    fn straight_line_path<E>(
        graph: &Graph<(), E>,
        edge: &Edge<E>,
        style: &GraphStyle,
        positions: &impl Fn(NodeId) -> Option<Vec2>,
        parallel: &[Option<(usize, usize)>],
    ) -> Vec<Bezier> {
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(400.0, 400.0));
        vp.zoom_at(Vec2::new(200.0, 200.0), 1.0);
        edge_path(
            edge,
            &ctx(0, 0.0, &[], parallel),
            graph,
            positions,
            &no_clusters(),
            &vp,
            style,
        )
    }

    #[test]
    fn short_edge_is_straight_when_threshold_enabled() {
        let g = graph();
        let ids: Vec<NodeId> = g.nodes().map(|(id, _)| id).collect();
        let a = ids[0];
        let b = ids[1];
        // Chord ~100px on screen; threshold 200px forces straight-line LOD.
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else if id == b {
                Some(Vec2::new(100.0, 0.0))
            } else {
                None
            }
        };
        let style = GraphStyle::default().with_edge_straight_threshold(200.0);
        let edge = g.edge(ids_edge(&g)).expect("edge exists");
        let path = straight_line_path(&g, edge, &style, &positions, &[None]);
        assert_eq!(path.len(), 1, "straight edge is a single segment");
        // The degenerate control point sits at the chord midpoint, so the
        // trimmed path is collinear with the source-target chord.
        let (p0, p1, p2) = path[0];
        let unit = (p2 - p0).normalize();
        let normal = Vec2::new(-unit.y, unit.x);
        let offset = (p1 - (p0 + p2) * 0.5).dot(normal).abs();
        assert!(offset < 1e-3, "control point must lie on the chord");
    }

    #[test]
    fn long_edge_stays_curved_when_threshold_enabled() {
        let g = graph();
        let ids: Vec<NodeId> = g.nodes().map(|(id, _)| id).collect();
        let a = ids[0];
        let b = ids[1];
        // Chord ~1000px on screen; threshold 200px keeps the curve path.
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else if id == b {
                Some(Vec2::new(1000.0, 0.0))
            } else {
                None
            }
        };
        let style = GraphStyle::default().with_edge_straight_threshold(200.0);
        let edge = g.edge(ids_edge(&g)).expect("edge exists");
        let path = straight_line_path(&g, edge, &style, &positions, &[None]);
        assert_eq!(path.len(), 1);
        // The density bow for a lone edge is zero, so the control point also
        // lies on the chord; the path is identical to the straight case. This
        // only asserts the curved branch is taken and returns a valid segment.
        assert!(finite_chord_length(path[0].0, path[0].2).is_some());
    }

    #[test]
    fn straight_line_trim_matches_binary_search() {
        // The analytic straight-line trim must agree (within float tolerance)
        // with the general curve trim when the control point is the chord
        // midpoint, so the straight-LOD rendering and hit testing stay identical.
        let radius = 6.0;
        for (source, target) in [
            (Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0)),
            (Vec2::new(0.0, 0.0), Vec2::new(-80.0, 60.0)),
            (Vec2::new(10.0, -5.0), Vec2::new(0.0, 90.0)),
        ] {
            let analytic = straight_line_trim(source, target, radius);
            let control = (source + target) * 0.5;
            let binary = trim_curve_to_node_boundary(source, control, target, radius);
            for (got, want) in [
                (analytic.0, binary.0),
                (analytic.1, binary.1),
                (analytic.2, binary.2),
            ] {
                assert!(
                    (got - want).length() < 0.5,
                    "straight trim diverges: got {got:?}, want {want:?}"
                );
            }
        }
    }

    #[test]
    fn self_loop_is_never_simplified() {
        let mut g: Graph<(), ()> = Graph::new();
        let a = g.add_node(());
        g.add_edge(a, a, EdgeDirection::Directed, ());
        let positions = move |id: NodeId| {
            if id == a { Some(Vec2::ZERO) } else { None }
        };
        let style = GraphStyle::default().with_edge_straight_threshold(500.0);
        let edge = g.edge(g.edges().next().unwrap().0).expect("edge exists");
        let path = straight_line_path(&g, edge, &style, &positions, &[None]);
        // A self-loop keeps its two-segment onigiri path regardless of the
        // straight-line threshold.
        assert_eq!(path.len(), 2, "self-loop onigiri is not simplified");
    }

    #[test]
    fn short_edge_omitted_when_min_length_enabled() {
        let g = graph();
        let ids: Vec<NodeId> = g.nodes().map(|(id, _)| id).collect();
        let a = ids[0];
        let b = ids[1];
        // Chord ~5px on screen, below a 10px edge-omission threshold.
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else if id == b {
                Some(Vec2::new(5.0, 0.0))
            } else {
                None
            }
        };
        let style = GraphStyle::default().with_edge_min_length(10.0);
        let edge = g.edge(ids_edge(&g)).expect("edge exists");
        let path = straight_line_path(&g, edge, &style, &positions, &[None]);
        assert!(
            path.is_empty(),
            "an edge at or below the omission threshold must produce no path"
        );
    }

    #[test]
    fn longer_edge_kept_when_min_length_enabled() {
        let g = graph();
        let ids: Vec<NodeId> = g.nodes().map(|(id, _)| id).collect();
        let a = ids[0];
        let b = ids[1];
        // Chord ~30px on screen, above a 10px edge-omission threshold.
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else if id == b {
                Some(Vec2::new(30.0, 0.0))
            } else {
                None
            }
        };
        let style = GraphStyle::default().with_edge_min_length(10.0);
        let edge = g.edge(ids_edge(&g)).expect("edge exists");
        let path = straight_line_path(&g, edge, &style, &positions, &[None]);
        assert_eq!(
            path.len(),
            1,
            "an edge above the omission threshold must still render"
        );
    }

    #[test]
    fn self_loop_kept_when_min_length_enabled() {
        let mut g: Graph<(), ()> = Graph::new();
        let a = g.add_node(());
        g.add_edge(a, a, EdgeDirection::Directed, ());
        let positions = move |id: NodeId| {
            if id == a { Some(Vec2::ZERO) } else { None }
        };
        let style = GraphStyle::default().with_edge_min_length(500.0);
        let edge = g.edge(g.edges().next().unwrap().0).expect("edge exists");
        let path = straight_line_path(&g, edge, &style, &positions, &[None]);
        assert_eq!(
            path.len(),
            2,
            "a self-loop must never be omitted by edge-omission LOD"
        );
    }

    #[test]
    fn edge_omission_disabled_by_default() {
        let g = graph();
        let ids: Vec<NodeId> = g.nodes().map(|(id, _)| id).collect();
        let a = ids[0];
        let b = ids[1];
        // Default threshold (0.0) disables edge omission: even a short edge
        // (20 units between centers, 10 after trimming to the node boundary
        // gap) still produces a straight path. The distance must exceed twice
        // the boundary offset (`node_radius + gap`), or the trim legitimately
        // empties the path.
        let positions = move |id: NodeId| {
            if id == a {
                Some(Vec2::new(0.0, 0.0))
            } else if id == b {
                Some(Vec2::new(20.0, 0.0))
            } else {
                None
            }
        };
        let style = GraphStyle::default();
        let edge = g.edge(ids_edge(&g)).expect("edge exists");
        let path = straight_line_path(&g, edge, &style, &positions, &[None]);
        assert!(
            !path.is_empty(),
            "default style must not omit edges via edge_min_length"
        );
    }

    #[test]
    fn short_edge_arrow_omitted_when_threshold_enabled() {
        // A 30px on-screen chord below a 50px arrow LOD threshold is omitted.
        let style = GraphStyle::default().with_edge_arrow_min_length(50.0);
        assert!(
            edge_arrow_omitted(Vec2::new(0.0, 0.0), Vec2::new(30.0, 0.0), false, &style),
            "a short directed edge below the threshold must omit its arrow"
        );
    }

    #[test]
    fn long_edge_arrow_kept_when_threshold_enabled() {
        // A 100px on-screen chord above a 50px arrow LOD threshold keeps its arrow.
        let style = GraphStyle::default().with_edge_arrow_min_length(50.0);
        assert!(
            !edge_arrow_omitted(Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0), false, &style),
            "a long edge above the threshold must keep its arrow"
        );
    }

    #[test]
    fn self_loop_arrow_never_omitted() {
        // A self-loop keeps its arrow even far below the arrow LOD threshold: the
        // arrow is the loop's only direction cue.
        let style = GraphStyle::default().with_edge_arrow_min_length(50.0);
        assert!(
            !edge_arrow_omitted(Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0), true, &style),
            "a self-loop must never omit its arrow"
        );
    }

    #[test]
    fn arrow_omitted_disabled_by_default() {
        // The default threshold (0.0) disables arrow LOD: every directed edge
        // keeps its arrow, preserving prior rendering.
        let style = GraphStyle::default();
        assert!(
            !edge_arrow_omitted(Vec2::new(0.0, 0.0), Vec2::new(5.0, 0.0), false, &style),
            "default style must never omit arrows"
        );
    }

    fn ids_edge(g: &Graph<(), ()>) -> EdgeId {
        g.edges().next().unwrap().0
    }

    #[test]
    fn node_simplified_when_diameter_below_threshold() {
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        scene.merge(
            GraphBatch::new()
                .node("a".to_owned(), ())
                .node("b".to_owned(), ())
                .edge(
                    "ab".to_owned(),
                    "a".to_owned(),
                    "b".to_owned(),
                    EdgeDirection::Directed,
                    (),
                ),
        );
        let a = scene.node_id(&"a".to_owned()).unwrap();
        let b = scene.node_id(&"b".to_owned()).unwrap();
        scene.set_position(a, Vec2::new(0.0, 0.0));
        scene.set_position(b, Vec2::new(100.0, 0.0));
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(400.0, 400.0));
        vp.fit_bounds(
            WorldBounds {
                min: Vec2::splat(-200.0),
                max: Vec2::splat(200.0),
            },
            0.0,
        );
        // node_radius is 6.0, so the diameter is 12.0; a threshold of 12.0 marks
        // every node simplified.
        let style = GraphStyle::default().with_node_simplify_threshold(12.0);
        let mut rt = GraphRuntime::new();
        let synced = scene.sync_runtime(&mut rt);
        let frame = build_indexed_paint_frame(IndexedPaintFrameInput {
            synced: &synced,
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &style,
            selection: &Selection::new(),
            hover: &Hover::default(),
            node_overlay: None,
            edge_overlay: None,
        });
        assert!(!frame.nodes.is_empty(), "sample scene has visible nodes");
        assert!(
            frame.nodes.iter().all(|n| n.simplified),
            "nodes at or below the simplify diameter must be marked simplified"
        );
    }

    #[test]
    fn node_not_simplified_when_threshold_disabled() {
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        scene.merge(
            GraphBatch::new()
                .node("a".to_owned(), ())
                .node("b".to_owned(), ())
                .edge(
                    "ab".to_owned(),
                    "a".to_owned(),
                    "b".to_owned(),
                    EdgeDirection::Directed,
                    (),
                ),
        );
        let a = scene.node_id(&"a".to_owned()).unwrap();
        let b = scene.node_id(&"b".to_owned()).unwrap();
        scene.set_position(a, Vec2::new(0.0, 0.0));
        scene.set_position(b, Vec2::new(100.0, 0.0));
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(400.0, 400.0));
        vp.fit_bounds(
            WorldBounds {
                min: Vec2::splat(-200.0),
                max: Vec2::splat(200.0),
            },
            0.0,
        );
        // Default threshold (0.0) disables node simplification.
        let style = GraphStyle::default();
        let mut rt = GraphRuntime::new();
        let synced = scene.sync_runtime(&mut rt);
        let frame = build_indexed_paint_frame(IndexedPaintFrameInput {
            synced: &synced,
            node_label: &no_labels(),
            edge_label: &no_edge_labels(),
            viewport: &vp,
            style: &style,
            selection: &Selection::new(),
            hover: &Hover::default(),
            node_overlay: None,
            edge_overlay: None,
        });
        assert!(!frame.nodes.is_empty(), "sample scene has visible nodes");
        assert!(
            frame.nodes.iter().all(|n| !n.simplified),
            "default style must never simplify nodes"
        );
    }
}
