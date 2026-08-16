//! Initial placement policies (§13).
//!
//! Initial placement is distinct from layout. When graph exploration
//! introduces new nodes, randomly redistributing the entire graph creates poor
//! interaction stability. New nodes should initially appear near their
//! expansion origin.

use glam::Vec2;

use super::graph::LayoutState;

/// A policy for assigning an initial position to a newly introduced node (§13).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Placement {
    /// A random position in a bounded region.
    Random,
    /// A position near the given origin.
    Around(Vec2),
    /// The barycenter (average) of existing node positions.
    Barycenter,
    /// A fixed position.
    Fixed(Vec2),
}

impl Default for Placement {
    fn default() -> Self {
        Self::Around(Vec2::ZERO)
    }
}

impl Placement {
    /// Compute an initial position for a new node.
    ///
    /// `rng` is a deterministic pseudo-random source so placement is
    /// reproducible in tests and demonstrations.
    pub fn initial_position(&self, state: &LayoutState, rng: &mut Rng) -> Vec2 {
        match self {
            Placement::Random => {
                let spread = 200.0;
                Vec2::new(
                    (rng.next_f32() - 0.5) * 2.0 * spread,
                    (rng.next_f32() - 0.5) * 2.0 * spread,
                )
            }
            Placement::Around(origin) => {
                let jitter = 24.0;
                *origin
                    + Vec2::new(
                        (rng.next_f32() - 0.5) * 2.0 * jitter,
                        (rng.next_f32() - 0.5) * 2.0 * jitter,
                    )
            }
            Placement::Barycenter => {
                if state.positions.is_empty() {
                    Vec2::ZERO
                } else {
                    let sum: Vec2 = state.positions.iter().copied().sum();
                    sum / state.positions.len() as f32
                }
            }
            Placement::Fixed(pos) => *pos,
        }
    }
}

/// A small deterministic pseudo-random source (xorshift64*).
///
/// Kept local to avoid a dependency and to make placement reproducible.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Create a new RNG from a seed.
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0x9E3779B97F4A7C15 } else { seed },
        }
    }

    /// Produce the next `f32` in `[0, 1)`.
    pub fn next_f32(&mut self) -> f32 {
        self.state = self.state.wrapping_mul(0x2545F4914F6CDD1D);
        let x = (self.state >> 11) as u32;
        (x as f32) / (u32::MAX as f32)
    }
}

impl Default for Rng {
    fn default() -> Self {
        Self::new(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_placement_ignores_rng() {
        let state = LayoutState::new();
        let mut rng = Rng::new(1);
        let p = Placement::Fixed(Vec2::new(10.0, 20.0));
        assert_eq!(p.initial_position(&state, &mut rng), Vec2::new(10.0, 20.0));
    }

    #[test]
    fn barycenter_averages_existing_positions() {
        let mut state = LayoutState::new();
        state.positions = vec![Vec2::new(0.0, 0.0), Vec2::new(10.0, 10.0)];
        let mut rng = Rng::new(1);
        let p = Placement::Barycenter;
        assert_eq!(p.initial_position(&state, &mut rng), Vec2::new(5.0, 5.0));
    }

    #[test]
    fn barycenter_empty_is_origin() {
        let state = LayoutState::new();
        let mut rng = Rng::new(1);
        assert_eq!(
            Placement::Barycenter.initial_position(&state, &mut rng),
            Vec2::ZERO
        );
    }

    #[test]
    fn around_origin_is_near_origin() {
        let state = LayoutState::new();
        let mut rng = Rng::new(42);
        let p = Placement::Around(Vec2::new(100.0, 100.0));
        let pos = p.initial_position(&state, &mut rng);
        assert!((pos - Vec2::new(100.0, 100.0)).length() < 30.0);
    }

    #[test]
    fn rng_is_deterministic() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..100 {
            assert_eq!(a.next_f32(), b.next_f32());
        }
    }
}
