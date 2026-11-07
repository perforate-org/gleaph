//! Colour and label typography as the core names them.
//!
//! A graph style is authored once and drawn by whatever host is attached. Both the colour model and
//! the label typography therefore live here, in types the core owns: a host converts at its
//! boundary (`gpui-graph` to `Hsla`/`TextStyle`, a Rune host to its own), instead of the core
//! naming one toolkit's types and every other host inheriting them.
//!
//! The colour model is HSL with alpha, which is what graph styles are authored in (a hue, a
//! saturation, a lightness and an alpha) and what the existing defaults and themes already use.

/// A graph colour: hue, saturation, lightness and alpha.
///
/// The components are the authored values, not resolved pixels. A host converts to its own colour
/// space at paint time, so the same style renders consistently under different toolkits' transfer
/// functions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    /// Hue in turns, `0.0..1.0` (wrapping).
    pub h: f32,
    /// Saturation, `0.0..=1.0`.
    pub s: f32,
    /// Lightness, `0.0..=1.0`.
    pub l: f32,
    /// Alpha, `0.0..=1.0`.
    pub a: f32,
}

impl Color {
    /// Opaque white.
    pub const WHITE: Color = Color {
        h: 0.0,
        s: 0.0,
        l: 1.0,
        a: 1.0,
    };
    /// Opaque black.
    pub const BLACK: Color = Color {
        h: 0.0,
        s: 0.0,
        l: 0.0,
        a: 1.0,
    };
    /// Fully transparent.
    pub const TRANSPARENT: Color = Color {
        h: 0.0,
        s: 0.0,
        l: 0.0,
        a: 0.0,
    };

    /// This colour with `alpha` replacing its own.
    pub fn with_alpha(self, alpha: f32) -> Color {
        Color { a: alpha, ..self }
    }

    /// This colour with its alpha scaled by `factor` (clamped to `0.0..=1.0`).
    pub fn fade(self, factor: f32) -> Color {
        Color {
            a: (self.a * factor).clamp(0.0, 1.0),
            ..self
        }
    }

    /// The colour as sRGB with alpha, `0.0..=1.0` per channel.
    ///
    /// Provided so a host with an RGB colour type converts without reimplementing the HSL
    /// conversion (and disagreeing with the other host about it).
    pub fn to_srgba(self) -> [f32; 4] {
        let Color { h, s, l, a } = self;
        if s <= 0.0 {
            return [l, l, l, a];
        }
        let q = if l < 0.5 {
            l * (1.0 + s)
        } else {
            l + s - l * s
        };
        let p = 2.0 * l - q;
        let h = h.rem_euclid(1.0);
        let channel = |t: f32| {
            let t = t.rem_euclid(1.0);
            if t < 1.0 / 6.0 {
                p + (q - p) * 6.0 * t
            } else if t < 0.5 {
                q
            } else if t < 2.0 / 3.0 {
                p + (q - p) * (2.0 / 3.0 - t) * 6.0
            } else {
                p
            }
        };
        [
            channel(h + 1.0 / 3.0),
            channel(h),
            channel(h - 1.0 / 3.0),
            a,
        ]
    }
}

/// A colour from hue, saturation, lightness and alpha, in [`Color`]'s units.
pub fn hsla(h: f32, s: f32, l: f32, a: f32) -> Color {
    Color { h, s, l, a }
}

/// Opaque white, the common stroke and label colour.
pub fn white() -> Color {
    Color::WHITE
}

/// A font size, kept in the units the style was authored in.
///
/// `Rem` follows the host's root font size (the application's text scale); `Px` is absolute. The
/// core never resolves either to pixels: a host does that when it shapes text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontSize {
    /// A multiple of the host's root font size.
    Rem(f32),
    /// An absolute size in logical pixels.
    Px(f32),
}

impl FontSize {
    /// This size in logical pixels for a host whose root font size is `rem_px`.
    pub fn to_pixels(self, rem_px: f32) -> f32 {
        match self {
            FontSize::Rem(rem) => rem * rem_px,
            FontSize::Px(px) => px,
        }
    }
}

impl Default for FontSize {
    fn default() -> Self {
        FontSize::Rem(1.0)
    }
}

/// How a host draws graph labels: colour and typography, without naming a text stack.
///
/// The core decides *which* labels survive (priority budget, collision) and *where* they go; the
/// host decides how they are shaped and drawn. This type is the hand-off between the two.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LabelStyle {
    /// Label text colour.
    pub color: Color,
    /// Label font size.
    pub font_size: FontSize,
    /// CSS-style font weight (`400` regular, `600` semibold, ...).
    pub weight: f32,
}

impl Default for LabelStyle {
    fn default() -> Self {
        Self {
            color: Color::WHITE,
            font_size: FontSize::default(),
            weight: 400.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsla_is_the_identity_of_the_authored_components() {
        let color = hsla(0.6, 0.5, 0.6, 0.35);
        assert_eq!(color.h, 0.6);
        assert_eq!(color.s, 0.5);
        assert_eq!(color.l, 0.6);
        assert_eq!(color.a, 0.35);
    }

    #[test]
    fn alpha_helpers_replace_and_scale_alpha() {
        let color = hsla(0.1, 0.2, 0.3, 0.5);
        assert_eq!(color.with_alpha(0.9).a, 0.9);
        assert_eq!(color.with_alpha(0.9).h, 0.1, "only alpha changes");
        assert_eq!(color.fade(0.5).a, 0.25);
        assert_eq!(color.fade(4.0).a, 1.0, "alpha stays in range");
    }

    #[test]
    fn zero_saturation_is_grey() {
        assert_eq!(hsla(0.3, 0.0, 0.4, 1.0).to_srgba(), [0.4, 0.4, 0.4, 1.0]);
    }

    /// Channel-wise comparison: the conversion is float arithmetic, so a component that should be
    /// exactly zero lands within rounding instead.
    fn assert_channels(actual: [f32; 4], expected: [f32; 4]) {
        for (index, (lhs, rhs)) in actual.iter().zip(expected.iter()).enumerate() {
            assert!(
                (lhs - rhs).abs() < 1e-6,
                "channel {index}: {actual:?} != {expected:?}"
            );
        }
    }

    #[test]
    fn the_primaries_convert_to_the_expected_srgb_channels() {
        // hue 0 is red, 1/3 green, 2/3 blue at full saturation and half lightness.
        assert_channels(hsla(0.0, 1.0, 0.5, 1.0).to_srgba(), [1.0, 0.0, 0.0, 1.0]);
        assert_channels(
            hsla(1.0 / 3.0, 1.0, 0.5, 1.0).to_srgba(),
            [0.0, 1.0, 0.0, 1.0],
        );
        assert_channels(
            hsla(2.0 / 3.0, 1.0, 0.5, 1.0).to_srgba(),
            [0.0, 0.0, 1.0, 1.0],
        );
    }

    #[test]
    fn hue_wraps_outside_one_turn() {
        let a = hsla(0.25, 0.8, 0.5, 1.0).to_srgba();
        let b = hsla(2.25, 0.8, 0.5, 1.0).to_srgba();
        for (lhs, rhs) in a.iter().zip(b.iter()) {
            assert!((lhs - rhs).abs() < 1e-6, "hue wraps: {a:?} == {b:?}");
        }
    }

    #[test]
    fn font_size_resolves_against_the_host_root() {
        assert_eq!(FontSize::Rem(0.75).to_pixels(16.0), 12.0);
        assert_eq!(FontSize::Px(13.0).to_pixels(16.0), 13.0);
        assert_eq!(FontSize::default(), FontSize::Rem(1.0));
    }
}
