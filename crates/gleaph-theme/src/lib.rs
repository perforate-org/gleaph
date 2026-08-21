// Auto-generated from Gleaph design tokens
// Source: design-tokens/primitive.json, semantic-light.json, semantic-dark.json
// Do not edit manually — regenerate with: node scripts/generate-gpui-theme.js

use gpui::TextStyle;
use gpui::hsla;
use gpui_graph::GraphStyle;

/// Gleaph application theme (light / dark variants).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// Canvas / window background.
    pub background: gpui::Hsla,
    /// Surface / card / panel background.
    pub surface: gpui::Hsla,
    /// Primary text.
    pub text_primary: gpui::Hsla,
    /// Secondary / muted text.
    pub text_secondary: gpui::Hsla,
    /// Accent / primary action.
    pub accent: gpui::Hsla,
    /// Text on accent backgrounds.
    pub accent_text: gpui::Hsla,
    /// Default borders.
    pub border: gpui::Hsla,
    /// Focus ring.
    pub focus_ring: gpui::Hsla,
    /// Destructive action (error).
    pub destructive: gpui::Hsla,
    /// Text on destructive backgrounds.
    pub destructive_text: gpui::Hsla,
    /// Primitive color palette.
    pub color: ColorPalette,
    /// Spacing scale.
    pub space: SpaceScale,
    /// Border radii.
    pub radius: RadiusScale,
}

/// Sand + Blue + Clay primitive color palette.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorPalette {
    pub blue_300: gpui::Hsla,
    pub blue_400: gpui::Hsla,
    pub blue_500: gpui::Hsla,
    pub blue_600: gpui::Hsla,
    pub blue_700: gpui::Hsla,
    pub clay_300: gpui::Hsla,
    pub clay_400: gpui::Hsla,
    pub clay_500: gpui::Hsla,
    pub sand_0: gpui::Hsla,
    pub sand_100: gpui::Hsla,
    pub sand_200: gpui::Hsla,
    pub sand_300: gpui::Hsla,
    pub sand_400: gpui::Hsla,
    pub sand_50: gpui::Hsla,
    pub sand_500: gpui::Hsla,
    pub sand_600: gpui::Hsla,
    pub sand_700: gpui::Hsla,
    pub sand_800: gpui::Hsla,
    pub sand_900: gpui::Hsla,
    pub sand_950: gpui::Hsla,
}

/// Spacing primitive scale (px).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpaceScale {
    pub space_4: f32,
    pub space_6: f32,
}

/// Border radius primitive scale (px).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadiusScale {
    pub md: f32,
}

impl Theme {
    /// Light theme (default).
    pub fn light() -> Self {
        Self {
            background: hsla(0.1250, 0.1379, 0.8863, 1.0),
            surface: hsla(0.0000, 0.0000, 1.0000, 1.0),
            text_primary: hsla(0.0000, 0.0000, 0.1294, 1.0),
            text_secondary: hsla(0.0000, 0.0000, 0.3765, 1.0),
            accent: hsla(0.6256, 0.6522, 0.4510, 1.0),
            accent_text: hsla(0.0000, 0.0000, 1.0000, 1.0),
            border: hsla(0.0000, 0.0000, 0.8196, 1.0),
            focus_ring: hsla(0.6250, 0.8780, 0.6784, 1.0),
            destructive: hsla(0.0000, 0.8424, 0.6020, 1.0),
            destructive_text: hsla(0.5833, 0.4000, 0.9804, 1.0),
            color: ColorPalette {
                blue_300: hsla(0.6250, 0.8780, 0.6784, 1.0),
                blue_400: hsla(0.6255, 0.7807, 0.5529, 1.0),
                blue_500: hsla(0.6256, 0.6522, 0.4510, 1.0),
                blue_600: hsla(0.5800, 0.8719, 0.3980, 1.0),
                blue_700: hsla(0.6085, 1.0000, 0.5000, 1.0),
                clay_300: hsla(0.0180, 0.3524, 0.7941, 1.0),
                clay_400: hsla(0.0152, 0.2558, 0.8314, 1.0),
                clay_500: hsla(0.0496, 0.3032, 0.6961, 1.0),
                sand_0: hsla(0.0000, 0.0000, 1.0000, 1.0),
                sand_100: hsla(0.1250, 0.1379, 0.8863, 1.0),
                sand_200: hsla(0.0000, 0.0000, 0.8196, 1.0),
                sand_300: hsla(0.0000, 0.0000, 0.7569, 1.0),
                sand_400: hsla(0.0000, 0.0000, 0.6275, 1.0),
                sand_50: hsla(0.1111, 0.2308, 0.9490, 1.0),
                sand_500: hsla(0.0000, 0.0000, 0.5020, 1.0),
                sand_600: hsla(0.0000, 0.0000, 0.3765, 1.0),
                sand_700: hsla(0.0000, 0.0000, 0.2510, 1.0),
                sand_800: hsla(0.0000, 0.0000, 0.2157, 1.0),
                sand_900: hsla(0.0000, 0.0000, 0.1294, 1.0),
                sand_950: hsla(0.0000, 0.0000, 0.0863, 1.0),
            },
            space: SpaceScale {
                space_4: 16.0,
                space_6: 24.0,
            },
            radius: RadiusScale { md: 4.0 },
        }
    }

    /// Dark theme.
    pub fn dark() -> Self {
        Self {
            background: hsla(0.0000, 0.0000, 0.0863, 1.0),
            surface: hsla(0.0000, 0.0000, 0.1294, 1.0),
            text_primary: hsla(0.0000, 0.0000, 1.0000, 1.0),
            text_secondary: hsla(0.0000, 0.0000, 0.7569, 1.0),
            accent: hsla(0.6255, 0.7807, 0.5529, 1.0),
            accent_text: hsla(0.0000, 0.0000, 1.0000, 1.0),
            border: hsla(0.0000, 0.0000, 0.2510, 1.0),
            focus_ring: hsla(0.6250, 0.8780, 0.6784, 1.0),
            destructive: hsla(0.0000, 0.8424, 0.6020, 1.0),
            destructive_text: hsla(0.5833, 0.4000, 0.9804, 1.0),
            color: ColorPalette {
                blue_300: hsla(0.6250, 0.8780, 0.6784, 1.0),
                blue_400: hsla(0.6255, 0.7807, 0.5529, 1.0),
                blue_500: hsla(0.6256, 0.6522, 0.4510, 1.0),
                blue_600: hsla(0.5800, 0.8719, 0.3980, 1.0),
                blue_700: hsla(0.6085, 1.0000, 0.5000, 1.0),
                clay_300: hsla(0.0180, 0.3524, 0.7941, 1.0),
                clay_400: hsla(0.0152, 0.2558, 0.8314, 1.0),
                clay_500: hsla(0.0496, 0.3032, 0.6961, 1.0),
                sand_0: hsla(0.0000, 0.0000, 1.0000, 1.0),
                sand_100: hsla(0.1250, 0.1379, 0.8863, 1.0),
                sand_200: hsla(0.0000, 0.0000, 0.8196, 1.0),
                sand_300: hsla(0.0000, 0.0000, 0.7569, 1.0),
                sand_400: hsla(0.0000, 0.0000, 0.6275, 1.0),
                sand_50: hsla(0.1111, 0.2308, 0.9490, 1.0),
                sand_500: hsla(0.0000, 0.0000, 0.5020, 1.0),
                sand_600: hsla(0.0000, 0.0000, 0.3765, 1.0),
                sand_700: hsla(0.0000, 0.0000, 0.2510, 1.0),
                sand_800: hsla(0.0000, 0.0000, 0.2157, 1.0),
                sand_900: hsla(0.0000, 0.0000, 0.1294, 1.0),
                sand_950: hsla(0.0000, 0.0000, 0.0863, 1.0),
            },
            space: SpaceScale {
                space_4: 16.0,
                space_6: 24.0,
            },
            radius: RadiusScale { md: 4.0 },
        }
    }

    /// Build a graph visualization style from this theme.
    pub fn graph_style(&self) -> GraphStyle {
        GraphStyle::default()
            .with_node_fill(self.accent)
            .with_node_stroke_color(self.border)
            .with_node_stroke_width(1.0)
            .with_node_fill_selected(self.color.blue_400)
            .with_node_fill_hovered(self.color.blue_300)
            .with_edge_color(self.text_secondary)
            .with_edge_width(1.5)
            .with_edge_color_selected(self.accent)
            .with_edge_color_hovered(self.color.blue_300)
            .with_edge_arrow_enabled(true)
            .with_edge_arrow_size(8.0)
            .with_label_style(TextStyle {
                color: self.text_primary,
                ..TextStyle::default()
            })
            .with_label_offset(4.0)
    }
}
