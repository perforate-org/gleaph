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
    /// Edge width in pixels.
    pub edge_width: f32,
    /// Edge color.
    pub edge_color: Hsla,
    /// Edge color when selected.
    pub edge_color_selected: Hsla,
    /// Edge color when hovered.
    pub edge_color_hovered: Hsla,
    /// Whether directed edges render an arrowhead.
    pub edge_arrow_enabled: bool,
    /// Arrowhead size in pixels (length along the edge).
    pub edge_arrow_size: f32,
    /// Arrowhead shape.
    pub edge_arrow_shape: ArrowShape,
    /// Text style for node and edge labels.
    pub label_style: TextStyle,
    /// Vertical offset of a node label below the node, in pixels.
    pub label_offset: f32,
}

impl Default for GraphStyle {
    fn default() -> Self {
        Self {
            node_radius: 6.0,
            node_fill: hsla(0.6, 0.5, 0.6, 1.0),
            node_stroke_width: 1.0,
            node_stroke_color: hsla(0.0, 0.0, 0.1, 1.0),
            node_fill_selected: hsla(0.08, 0.7, 0.55, 1.0),
            node_fill_hovered: hsla(0.6, 0.5, 0.7, 1.0),
            edge_width: 1.5,
            edge_color: hsla(0.0, 0.0, 0.5, 1.0),
            edge_color_selected: hsla(0.08, 0.7, 0.55, 1.0),
            edge_color_hovered: hsla(0.0, 0.0, 0.7, 1.0),
            edge_arrow_enabled: true,
            edge_arrow_size: 8.0,
            edge_arrow_shape: ArrowShape::Triangle,
            label_style: TextStyle::default(),
            label_offset: 0.0,
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
}
