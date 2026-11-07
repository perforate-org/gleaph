//! The colour and label-typography bridge between the core's vocabulary and GPUI's.
//!
//! The core authors graph styles in its own [`Color`](graph_view_core::Color) and
//! [`LabelStyle`](graph_view_core::LabelStyle) so a Rune host can draw the same style. GPUI paints
//! with `Hsla` and shapes text from a `TextStyle`, so this module is the one place the two meet -
//! the conversion lives here rather than at every paint call, so both stay in step.

use gpui::{FontWeight, Hsla, TextStyle, hsla, px, rems};
use graph_view_core::{Color, FontSize, LabelStyle};

/// A core colour as GPUI paints it.
pub fn to_hsla(color: Color) -> Hsla {
    hsla(color.h, color.s, color.l, color.a)
}

/// A core label style as GPUI shapes it.
///
/// GPUI resolves `Rem` against the window's root size when it paints, so the size stays in the
/// style's own units here; only [`LabelStyle::font_size`]'s unit changes.
pub fn to_text_style(style: &LabelStyle) -> TextStyle {
    TextStyle {
        color: to_hsla(style.color),
        font_size: match style.font_size {
            FontSize::Rem(rem) => rems(rem).into(),
            FontSize::Px(px_size) => px(px_size).into(),
        },
        font_weight: FontWeight(style.weight),
        ..TextStyle::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_view_core::FontSize;

    #[test]
    fn a_core_colour_becomes_the_same_hsla() {
        let color = Color {
            h: 0.6,
            s: 0.5,
            l: 0.4,
            a: 0.35,
        };
        let converted = to_hsla(color);
        assert_eq!(converted, hsla(0.6, 0.5, 0.4, 0.35));
    }

    #[test]
    fn rem_and_pixel_sizes_keep_their_meaning() {
        let rem = to_text_style(&LabelStyle {
            font_size: FontSize::Rem(0.75),
            ..LabelStyle::default()
        });
        assert_eq!(rem.font_size, rems(0.75).into());
        let absolute = to_text_style(&LabelStyle {
            font_size: FontSize::Px(13.0),
            ..LabelStyle::default()
        });
        assert_eq!(absolute.font_size, px(13.0).into());
    }

    #[test]
    fn colour_and_weight_reach_the_text_style() {
        let style = LabelStyle {
            color: Color::WHITE,
            font_size: FontSize::default(),
            weight: 600.0,
        };
        let converted = to_text_style(&style);
        assert_eq!(converted.color, hsla(0.0, 0.0, 1.0, 1.0));
        assert_eq!(converted.font_weight, FontWeight(600.0));
    }
}
