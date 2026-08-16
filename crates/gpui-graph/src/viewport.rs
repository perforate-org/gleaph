//! Viewport (§17).
//!
//! The renderer operates in graph world coordinates. Canvas-local pixels are
//! introduced only through the viewport transformation. Window-space conversion
//! belongs to the `GraphView` boundary, and layout algorithms must not depend on
//! `gpui::Pixels` (Invariant 5).

use glam::Vec2;

/// An axis-aligned rectangle in world coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldBounds {
    /// Minimum corner.
    pub min: Vec2,
    /// Maximum corner.
    pub max: Vec2,
}

impl WorldBounds {
    /// The size of the bounds.
    pub fn size(&self) -> Vec2 {
        self.max - self.min
    }

    /// The center of the bounds.
    pub fn center(&self) -> Vec2 {
        (self.min + self.max) * 0.5
    }

    /// Whether the bounds are empty (zero or negative size).
    pub fn is_empty(&self) -> bool {
        self.size().x <= 0.0 || self.size().y <= 0.0
    }
}

/// A view into a graph scene in world coordinates (§17).
///
/// `zoom` is measured in pixels per world unit. Screen coordinates are
/// canvas-local pixels (`f32`); the `GraphView` layer translates them to and
/// from window-space `gpui::Pixels` at the GPUI boundary.
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    /// World coordinate at the center of the view.
    center: Vec2,
    /// Pixels per world unit.
    zoom: f32,
    /// Viewport size in pixels.
    size: Vec2,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            center: Vec2::ZERO,
            zoom: 1.0,
            size: Vec2::ZERO,
        }
    }
}

impl Viewport {
    /// Create a viewport with default state.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current zoom (pixels per world unit).
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// The world coordinate at the center of the view.
    pub fn center(&self) -> Vec2 {
        self.center
    }

    /// The viewport size in pixels.
    pub fn size(&self) -> Vec2 {
        self.size
    }

    /// Set the viewport size in pixels.
    pub fn set_size(&mut self, size: Vec2) {
        self.size = size;
    }

    /// Convert a world coordinate to a screen (pixel) coordinate.
    pub fn world_to_screen(&self, world: Vec2) -> Vec2 {
        (world - self.center) * self.zoom + self.size * 0.5
    }

    /// Convert a screen (pixel) coordinate to a world coordinate.
    pub fn screen_to_world(&self, screen: Vec2) -> Vec2 {
        (screen - self.size * 0.5) / self.zoom + self.center
    }

    /// Pan the view by a screen-space delta.
    pub fn pan(&mut self, delta_screen: Vec2) {
        self.center -= delta_screen / self.zoom;
    }

    /// Zoom by `factor`, keeping the world point under `screen_point` fixed.
    pub fn zoom_at(&mut self, screen_point: Vec2, factor: f32) {
        let world = self.screen_to_world(screen_point);
        self.zoom = (self.zoom * factor).clamp(0.0001, 1.0e6);
        self.center = world - (screen_point - self.size * 0.5) / self.zoom;
    }

    /// The world bounds currently visible in the viewport.
    pub fn visible_world_bounds(&self) -> WorldBounds {
        WorldBounds {
            min: self.screen_to_world(Vec2::ZERO),
            max: self.screen_to_world(self.size),
        }
    }

    /// Fit the given world bounds into the viewport, optionally with padding.
    ///
    /// If the bounds are empty, the view is centered on the bounds center at
    /// the current zoom.
    pub fn fit_bounds(&mut self, bounds: WorldBounds, padding: f32) {
        if bounds.is_empty() {
            self.center = bounds.center();
            return;
        }
        let size = bounds.size();
        let zoom = (self.size.x / size.x).min(self.size.y / size.y) * (1.0 - padding);
        self.zoom = zoom.max(0.0001);
        self.center = bounds.center();
    }

    /// Center the view on a world point without changing zoom.
    pub fn focus(&mut self, world: Vec2) {
        self.center = world;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_world_screen() {
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(800.0, 600.0));
        vp.center = Vec2::new(10.0, 20.0);
        vp.zoom = 2.0;

        let world = Vec2::new(15.0, 25.0);
        let screen = vp.world_to_screen(world);
        let back = vp.screen_to_world(screen);
        assert!((back - world).length() < 1e-4);
    }

    #[test]
    fn zoom_at_keeps_anchor_fixed() {
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(800.0, 600.0));
        vp.center = Vec2::ZERO;
        vp.zoom = 1.0;

        let anchor_screen = Vec2::new(200.0, 150.0);
        let anchor_world = vp.screen_to_world(anchor_screen);
        vp.zoom_at(anchor_screen, 2.0);
        let after = vp.screen_to_world(anchor_screen);
        assert!((after - anchor_world).length() < 1e-3);
        assert!((vp.zoom - 2.0).abs() < 1e-4);
    }

    #[test]
    fn pan_moves_center() {
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(800.0, 600.0));
        vp.center = Vec2::ZERO;
        vp.zoom = 1.0;
        vp.pan(Vec2::new(100.0, 0.0));
        assert_eq!(vp.center, Vec2::new(-100.0, 0.0));
    }

    #[test]
    fn fit_bounds_sets_zoom_and_center() {
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(800.0, 600.0));
        let bounds = WorldBounds {
            min: Vec2::new(0.0, 0.0),
            max: Vec2::new(100.0, 100.0),
        };
        vp.fit_bounds(bounds, 0.0);
        assert_eq!(vp.center, Vec2::new(50.0, 50.0));
        assert!((vp.zoom - 6.0).abs() < 1e-3); // min(800/100, 600/100) = 6
    }

    #[test]
    fn visible_bounds_round_trip() {
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(800.0, 600.0));
        vp.center = Vec2::new(5.0, 5.0);
        vp.zoom = 2.0;
        let b = vp.visible_world_bounds();
        assert_eq!(b.min, vp.screen_to_world(Vec2::ZERO));
        assert_eq!(b.max, vp.screen_to_world(vp.size));
    }
}
