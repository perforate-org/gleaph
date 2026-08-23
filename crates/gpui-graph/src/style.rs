//! Graph styling (§26.2).
//!
//! Graph-specific appearance (node radius, fill, stroke, edge width, ...) is
//! distinct from GPUI element styling (§26.1). The style reuses GPUI types
//! (`Hsla`, `TextStyle`) so graph appearance and text share a single color and
//! font vocabulary with the rest of the application.

use gpui::{Hsla, TextStyle, hsla};

/// The shape of a directed edge's arrowhead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowShape {
    /// A filled triangle pointing along the edge.
    Triangle,
    /// An open chevron (two lines) pointing along the edge.
    Line,
    /// A filled circle at the target end.
    Circle,
}

/// Graph-specific appearance settings (§26.2).
#[derive(Debug, Clone, PartialEq)]
pub struct GraphStyle {
    /// Node radius in pixels.
    pub node_radius: f32,
    /// On-screen diameter in pixels below which a node renders simplified (fill
    /// only, no stroke).
    ///
    /// At small on-screen size a node's stroke is a sub-pixel ring that costs a
    /// quad primitive's stroke work for no visible outline, so dropping it keeps
    /// the node readable as a filled dot while reducing paint cost. A value of
    /// `0.0` (the default) disables the simplification and always draws the
    /// stroke. The threshold is a diameter, so a node whose visible diameter is
    /// at or below it renders fill-only.
    pub node_simplify_threshold: f32,
    /// Minimum on-screen radius in pixels below which a node never shrinks.
    ///
    /// Nodes are world-sized, so at deep zoom-out their on-screen radius
    /// approaches zero and the node vanishes. This floor keeps every node a
    /// visible marker dot and an hittable target once its world size drops
    /// below the floor. It applies to drawing and hit testing only; curve
    /// geometry stays in world units and is unaffected. A value of `0.0` (the
    /// default) disables the floor.
    pub node_min_screen_radius: f32,
    /// Node fill color.
    pub node_fill: Hsla,
    /// Node stroke width in pixels.
    pub node_stroke_width: f32,
    /// Node stroke color.
    pub node_stroke_color: Hsla,
    /// Node fill color when selected.
    pub node_fill_selected: Hsla,
    /// Node fill color when hovered.
    pub node_fill_hovered: Hsla,
    /// Node fill color under an `OverlayCategory::Emphasized` or `Accent` query
    /// overlay. Kept distinct from selection so query emphasis and selection
    /// compose without overwriting each other (§10).
    pub node_fill_overlay: Hsla,
    /// Node fill color under an `OverlayCategory::Dimmed` overlay.
    pub node_fill_muted: Hsla,
    /// Edge width in pixels.
    pub edge_width: f32,
    /// Edge color.
    pub edge_color: Hsla,
    /// Edge color when selected.
    pub edge_color_selected: Hsla,
    /// Edge color when hovered.
    pub edge_color_hovered: Hsla,
    /// Edge color under an `OverlayCategory::Emphasized` or `Accent` query
    /// overlay.
    pub edge_color_overlay: Hsla,
    /// Edge color under an `OverlayCategory::Dimmed` overlay.
    pub edge_color_muted: Hsla,
    /// Whether directed edges render an arrowhead.
    pub edge_arrow_enabled: bool,
    /// Arrowhead size in pixels (length along the edge).
    pub edge_arrow_size: f32,
    /// Arrowhead shape.
    pub edge_arrow_shape: ArrowShape,
    /// On-screen length in pixels below which a directed, non-self-loop edge's
    /// arrowhead is omitted.
    ///
    /// A very short edge carries no readable direction anyway, and each arrow is
    /// an independent painted primitive, so dropping these arrowheads removes
    /// GPU/primitive work in zoomed-out views where most edges are far shorter
    /// than the arrowhead itself. A value of `0.0` (the default) disables the
    /// simplification and always draws arrowheads for enabled directed edges.
    /// Self-loops are never omitted: their arrow is the only indication of
    /// direction and the loop has no short-chord case. The value should usually
    /// be at least `edge_arrow_size` so an omitted arrow is actually smaller
    /// than the edges that keep theirs.
    pub edge_arrow_min_length: f32,
    /// On-screen length in pixels below which a non-self-loop edge is omitted
    /// entirely, producing no edge or arrow primitive at all.
    ///
    /// An edge this short is visually a sub-pixel dot between two (already tiny)
    /// nodes, so painting it wastes a stroke primitive (and any arrowhead) with
    /// no readable geometry. A value of `0.0` (the default) disables the
    /// simplification and always paints eligible edges. Self-loops are never
    /// omitted. This is a distinct axis from [`Self::edge_straight_threshold`]
    /// (straighten but keep painting) and [`Self::edge_arrow_min_length`]
    /// (keep the edge but drop its arrow); it should usually be set below both
    /// so the view passes through straighten-then-arrow-then-omit as it zooms
    /// out.
    pub edge_min_length: f32,
    /// Text style for node and edge labels.
    pub label_style: TextStyle,
    /// Vertical offset of a node label below the node, in pixels.
    pub label_offset: f32,
    /// Distance in pixels below which an edge label is hidden because it has
    /// drifted too close to a node center. This prevents a label from sitting
    /// on top of a node after sliding along its edge.
    pub edge_label_hide_distance: f32,
    /// On-screen length in pixels below which a non-self-loop edge is rendered
    /// as a straight line instead of a density/cluster/obstacle-avoiding curve.
    ///
    /// When an edge is this short on screen its curvature and obstacle bow are
    /// visually indistinguishable from a straight line, so level-of-detail
    /// simplification drops the per-edge curve computation entirely. A value of
    /// `0.0` (the default) disables the simplification and always renders
    /// curves. Self-loops are never simplified.
    pub edge_straight_threshold: f32,
    /// On-screen length in pixels below which a non-self-loop edge is rendered
    /// as a straight line **while the user is interacting** (panning or
    /// zooming) and for a short settling period after the interaction ends.
    ///
    /// Interaction-time LOD keeps pan/zoom smooth on large graphs: while the
    /// camera is moving, per-edge curve computation is visually unnecessary and
    /// dominates the frame, so raising the straight threshold collapses every
    /// visible edge to a cheap straight segment. After the interaction stops,
    /// the frame continues to use this threshold for [`Self::edge_settle_time_ms`]
    /// so detail does not pop back the instant the camera stops; only once the
    /// settle elapses does the view return to [`Self::edge_straight_threshold`].
    ///
    /// A value of `0.0` (the default) disables interaction-time LOD entirely and
    /// the `edge_straight_threshold` is always used. When nonzero it should be
    /// larger than `edge_straight_threshold` to force more edges straight while
    /// moving. Self-loops are never simplified.
    pub edge_straight_threshold_while_interacting: f32,
    /// Duration, in milliseconds, that the straight-line threshold stays at
    /// [`Self::edge_straight_threshold_while_interacting`] after the last pan or
    /// zoom event, before settling back to [`Self::edge_straight_threshold`].
    ///
    /// This settling period adds hysteresis around the end of an interaction so
    /// detail does not pop back the moment the camera stops; a short settle lets
    /// the zoomed view stabilize in low detail before re-computing curves. It is
    /// ignored when `edge_straight_threshold_while_interacting` is `0.0`.
    pub edge_settle_time_ms: f32,
}

impl Default for GraphStyle {
    fn default() -> Self {
        Self {
            node_radius: 6.0,
            node_simplify_threshold: 0.0,
            node_min_screen_radius: 4.0,
            node_fill: hsla(0.6, 0.5, 0.6, 1.0),
            node_stroke_width: 1.0,
            node_stroke_color: hsla(0.0, 0.0, 0.1, 1.0),
            node_fill_selected: hsla(0.08, 0.7, 0.55, 1.0),
            node_fill_hovered: hsla(0.6, 0.5, 0.7, 1.0),
            node_fill_overlay: hsla(0.08, 0.7, 0.55, 1.0),
            node_fill_muted: hsla(0.6, 0.1, 0.6, 0.35),
            edge_width: 1.5,
            edge_color: hsla(0.0, 0.0, 0.5, 1.0),
            edge_color_selected: hsla(0.08, 0.7, 0.55, 1.0),
            edge_color_hovered: hsla(0.0, 0.0, 0.7, 1.0),
            edge_color_overlay: hsla(0.08, 0.7, 0.55, 1.0),
            edge_color_muted: hsla(0.0, 0.0, 0.5, 0.25),
            edge_arrow_enabled: true,
            edge_arrow_size: 8.0,
            edge_arrow_shape: ArrowShape::Triangle,
            edge_arrow_min_length: 0.0,
            edge_min_length: 0.0,
            label_style: TextStyle::default(),
            label_offset: 0.0,
            edge_label_hide_distance: 20.0,
            edge_straight_threshold: 0.0,
            edge_straight_threshold_while_interacting: 0.0,
            edge_settle_time_ms: 0.0,
        }
    }
}

impl GraphStyle {
    /// Set the node radius.
    pub fn with_node_radius(mut self, radius: f32) -> Self {
        self.node_radius = radius;
        self
    }

    /// Set the on-screen diameter below which a node renders simplified (fill
    /// only). See [`Self::node_simplify_threshold`].
    pub fn with_node_simplify_threshold(mut self, diameter: f32) -> Self {
        self.node_simplify_threshold = diameter;
        self
    }

    /// Set the minimum on-screen node radius. See
    /// [`Self::node_min_screen_radius`].
    pub fn with_node_min_screen_radius(mut self, radius: f32) -> Self {
        self.node_min_screen_radius = radius;
        self
    }

    /// Set the node fill color.
    pub fn with_node_fill(mut self, fill: Hsla) -> Self {
        self.node_fill = fill;
        self
    }

    /// Set the node fill color when hovered.
    pub fn with_node_fill_hovered(mut self, fill: Hsla) -> Self {
        self.node_fill_hovered = fill;
        self
    }

    /// Set the node fill color when selected.
    pub fn with_node_fill_selected(mut self, fill: Hsla) -> Self {
        self.node_fill_selected = fill;
        self
    }

    /// Set the node fill color under an emphasized or accent overlay.
    pub fn with_node_fill_overlay(mut self, fill: Hsla) -> Self {
        self.node_fill_overlay = fill;
        self
    }

    /// Set the node fill color under a dimmed overlay.
    pub fn with_node_fill_muted(mut self, fill: Hsla) -> Self {
        self.node_fill_muted = fill;
        self
    }

    /// Set the node stroke width.
    pub fn with_node_stroke_width(mut self, width: f32) -> Self {
        self.node_stroke_width = width;
        self
    }

    /// Set the node stroke color.
    pub fn with_node_stroke_color(mut self, color: Hsla) -> Self {
        self.node_stroke_color = color;
        self
    }

    /// Set the edge width.
    pub fn with_edge_width(mut self, width: f32) -> Self {
        self.edge_width = width;
        self
    }

    /// Set the edge color.
    pub fn with_edge_color(mut self, color: Hsla) -> Self {
        self.edge_color = color;
        self
    }

    /// Set the edge color when hovered.
    pub fn with_edge_color_hovered(mut self, color: Hsla) -> Self {
        self.edge_color_hovered = color;
        self
    }

    /// Set the edge color when selected.
    pub fn with_edge_color_selected(mut self, color: Hsla) -> Self {
        self.edge_color_selected = color;
        self
    }

    /// Set the edge color under an emphasized or accent overlay.
    pub fn with_edge_color_overlay(mut self, color: Hsla) -> Self {
        self.edge_color_overlay = color;
        self
    }

    /// Set the edge color under a dimmed overlay.
    pub fn with_edge_color_muted(mut self, color: Hsla) -> Self {
        self.edge_color_muted = color;
        self
    }

    /// Set whether directed edges render an arrowhead.
    pub fn with_edge_arrow_enabled(mut self, enabled: bool) -> Self {
        self.edge_arrow_enabled = enabled;
        self
    }

    /// Set the arrowhead size in pixels.
    pub fn with_edge_arrow_size(mut self, size: f32) -> Self {
        self.edge_arrow_size = size;
        self
    }

    /// Set the arrowhead shape.
    pub fn with_edge_arrow_shape(mut self, shape: ArrowShape) -> Self {
        self.edge_arrow_shape = shape;
        self
    }

    /// Set the minimum on-screen edge length below which a directed,
    /// non-self-loop edge's arrowhead is omitted. See
    /// [`Self::edge_arrow_min_length`].
    pub fn with_edge_arrow_min_length(mut self, pixels: f32) -> Self {
        self.edge_arrow_min_length = pixels;
        self
    }

    /// Set the on-screen length below which a non-self-loop edge is omitted
    /// entirely. See [`Self::edge_min_length`].
    pub fn with_edge_min_length(mut self, pixels: f32) -> Self {
        self.edge_min_length = pixels;
        self
    }

    /// Set the label text style.
    pub fn with_label_style(mut self, style: TextStyle) -> Self {
        self.label_style = style;
        self
    }

    /// Set the node label vertical offset in pixels.
    pub fn with_label_offset(mut self, offset: f32) -> Self {
        self.label_offset = offset;
        self
    }

    /// Set the distance below which an edge label is hidden because it has
    /// drifted too close to a node center.
    pub fn with_edge_label_hide_distance(mut self, distance: f32) -> Self {
        self.edge_label_hide_distance = distance;
        self
    }

    /// Set the on-screen length below which a non-self-loop edge is rendered as
    /// a straight line instead of a curve. `0.0` disables the simplification.
    pub fn with_edge_straight_threshold(mut self, pixels: f32) -> Self {
        self.edge_straight_threshold = pixels;
        self
    }

    /// Set the straight-line threshold used while the user is interacting
    /// (panning/zooming) and for the settle period after it stops. `0.0` disables
    /// interaction-time LOD. See [`Self::edge_straight_threshold_while_interacting`].
    pub fn with_edge_straight_threshold_while_interacting(mut self, pixels: f32) -> Self {
        self.edge_straight_threshold_while_interacting = pixels;
        self
    }

    /// Set the settle period, in milliseconds, during which the interaction-time
    /// straight threshold stays active after the last pan/zoom event.
    pub fn with_edge_settle_time(mut self, milliseconds: f32) -> Self {
        self.edge_settle_time_ms = milliseconds;
        self
    }
}
