//! Graph styling (§26.2).
//!
//! Graph-specific appearance (node radius, fill, stroke, edge width, ...) is
//! distinct from GPUI element styling (§26.1). The style is kept independent of
//! GPUI types so the paint layer can be tested without a running GPUI app; the
//! view layer converts these to GPUI colors.

/// An RGBA color in `[0, 1]` components.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    /// Red.
    pub r: f32,
    /// Green.
    pub g: f32,
    /// Blue.
    pub b: f32,
    /// Alpha.
    pub a: f32,
}

impl Rgba {
    /// A fully opaque color.
    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    /// A fully transparent black.
    pub const TRANSPARENT: Self = Self {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };
}

/// A stroke (outline) style.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stroke {
    /// Stroke width in pixels.
    pub width: f32,
    /// Stroke color.
    pub color: Rgba,
}

impl Stroke {
    /// A stroke with the given width and color.
    pub const fn new(width: f32, color: Rgba) -> Self {
        Self { width, color }
    }
}

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
    /// Node fill color.
    pub node_fill: Rgba,
    /// Node stroke.
    pub node_stroke: Stroke,
    /// Node fill color when selected.
    pub node_fill_selected: Rgba,
    /// Node fill color when hovered.
    pub node_fill_hovered: Rgba,
    /// Edge width in pixels.
    pub edge_width: f32,
    /// Edge color.
    pub edge_color: Rgba,
    /// Edge color when selected.
    pub edge_color_selected: Rgba,
    /// Edge color when hovered.
    pub edge_color_hovered: Rgba,
    /// Whether directed edges render an arrowhead.
    pub edge_arrow_enabled: bool,
    /// Arrowhead size in pixels (length along the edge).
    pub edge_arrow_size: f32,
    /// Arrowhead shape.
    pub edge_arrow_shape: ArrowShape,
}

impl Default for GraphStyle {
    fn default() -> Self {
        Self {
            node_radius: 6.0,
            node_fill: Rgba::rgb(0.35, 0.55, 0.9),
            node_stroke: Stroke::new(1.0, Rgba::rgb(0.1, 0.1, 0.1)),
            node_fill_selected: Rgba::rgb(0.9, 0.5, 0.2),
            node_fill_hovered: Rgba::rgb(0.5, 0.7, 0.95),
            edge_width: 1.5,
            edge_color: Rgba::rgb(0.5, 0.5, 0.5),
            edge_color_selected: Rgba::rgb(0.9, 0.5, 0.2),
            edge_color_hovered: Rgba::rgb(0.7, 0.7, 0.7),
            edge_arrow_enabled: true,
            edge_arrow_size: 8.0,
            edge_arrow_shape: ArrowShape::Triangle,
        }
    }
}

impl GraphStyle {
    /// Set the node radius.
    pub fn with_node_radius(mut self, radius: f32) -> Self {
        self.node_radius = radius;
        self
    }

    /// Set the node fill color.
    pub fn with_node_fill(mut self, fill: Rgba) -> Self {
        self.node_fill = fill;
        self
    }

    /// Set the node fill color when hovered.
    pub fn with_node_fill_hovered(mut self, fill: Rgba) -> Self {
        self.node_fill_hovered = fill;
        self
    }

    /// Set the node fill color when selected.
    pub fn with_node_fill_selected(mut self, fill: Rgba) -> Self {
        self.node_fill_selected = fill;
        self
    }

    /// Set the node stroke.
    pub fn with_node_stroke(mut self, stroke: Stroke) -> Self {
        self.node_stroke = stroke;
        self
    }

    /// Set the edge width.
    pub fn with_edge_width(mut self, width: f32) -> Self {
        self.edge_width = width;
        self
    }

    /// Set the edge color.
    pub fn with_edge_color(mut self, color: Rgba) -> Self {
        self.edge_color = color;
        self
    }

    /// Set the edge color when hovered.
    pub fn with_edge_color_hovered(mut self, color: Rgba) -> Self {
        self.edge_color_hovered = color;
        self
    }

    /// Set the edge color when selected.
    pub fn with_edge_color_selected(mut self, color: Rgba) -> Self {
        self.edge_color_selected = color;
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
}
