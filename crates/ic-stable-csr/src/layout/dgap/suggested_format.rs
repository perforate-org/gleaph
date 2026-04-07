//! Heuristic [`DgapSuggestedFormat`] for [`crate::dgap::DgapEdgeStore::format_new`] (`elem_capacity`, `segment_count`, `segment_size`).
//!
//! This mirrors the segment grid and initial slab sizing idea from `gleaph-old`’s `pma::compute_capacity`, adapted to DGAP V1 headers.
//!
//! # Not covered here
//!
//! - **`tree_height`** — [`crate::layout::dgap::DgapEdgeHeaderV1`] sets this inside [`crate::dgap::DgapEdgeStore::format_new`] via `floor_log2_u32(segment_count.max(1))`; do not duplicate.
//! - **Log pool** — `max_log_entries` stays [`super::DGAP_DEFAULT_MAX_LOG_ENTRIES`]. Heavy overflow workloads may need a different cap outside this helper.

/// Minimum initial CSR slab slot count from [`suggested_format`].
pub const SUGGESTED_MIN_ELEM_CAPACITY: u64 = 16;

/// Multiplier applied to `max(expected_edges, expected_vertices)` for initial [`DgapSuggestedFormat::elem_capacity`].
///
/// Rationale: same 4× headroom as legacy `gleaph-old` `compute_capacity`, reducing early [`crate::dgap::DgapEdgeStore::resize_double`] churn for typical graphs (not a guarantee for skewed degrees).
pub const SUGGESTED_ELEM_CAPACITY_MULTIPLIER: u64 = 4;

/// Arguments for [`crate::dgap::DgapEdgeStore::format_new`] except `num_edges` (callers pass current edge count separately).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DgapSuggestedFormat {
    pub elem_capacity: u64,
    pub segment_count: u32,
    pub segment_size: u32,
}

impl DgapSuggestedFormat {
    /// `(elem_capacity, segment_count, segment_size, num_edges)` for [`crate::dgap::DgapEdgeStore::format_new`].
    #[inline]
    pub fn format_new_tuple(self, num_edges: u64) -> (u64, u32, u32, u64) {
        (
            self.elem_capacity,
            self.segment_count,
            self.segment_size,
            num_edges,
        )
    }
}

/// `ceil(log2(x))`, with `x <= 1` → `0` (matches `gleaph-old` `pma::math::ceil_log2`).
#[inline]
fn ceil_log2(x: u64) -> u32 {
    if x <= 1 {
        return 0;
    }
    let f = 63 - x.leading_zeros();
    if x.is_power_of_two() { f } else { f + 1 }
}

#[inline]
fn ceil_div(n: u64, d: u64) -> u64 {
    debug_assert!(d != 0);
    n.div_ceil(d)
}

/// Heuristic `segment_size`, `segment_count`, and `elem_capacity` for a new DGAP edge region.
///
/// `expected_vertices` is treated as at least `1` for grid math (empty graph still needs a valid PMA grid).
///
/// **Invariant:** `segment_count * segment_size >= max(expected_vertices, 1)`.
#[inline]
pub fn suggested_format(expected_vertices: u64, expected_edges: u64) -> DgapSuggestedFormat {
    let v = expected_vertices.max(1);
    let segment_size = ceil_log2(v).max(1);
    let raw_count = ceil_div(v, u64::from(segment_size)).max(1);
    let segment_count = raw_count.next_power_of_two().max(1) as u32;
    let base = expected_edges.max(v);
    let elem_capacity = base
        .saturating_mul(SUGGESTED_ELEM_CAPACITY_MULTIPLIER)
        .max(SUGGESTED_MIN_ELEM_CAPACITY);
    DgapSuggestedFormat {
        elem_capacity,
        segment_count,
        segment_size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggested_format_matches_legacy_compute_capacity_shape() {
        let s = suggested_format(1000, 500);
        assert_eq!(s.segment_size, 10);
        assert_eq!(s.segment_count, 128);
        assert_eq!(s.elem_capacity, 4000);
    }

    #[test]
    fn suggested_format_segment_grid_covers_vertices() {
        for n in 1u64..=10_000 {
            let s = suggested_format(n, 0);
            let covered = u64::from(s.segment_count) * u64::from(s.segment_size);
            assert!(
                covered >= n,
                "n={n} segment_size={} segment_count={} covered={covered}",
                s.segment_size,
                s.segment_count
            );
        }
    }

    #[test]
    fn suggested_format_segment_count_is_power_of_two() {
        for n in [1u64, 2, 3, 7, 8, 9, 1000, u64::from(u32::MAX)] {
            let s = suggested_format(n, 0);
            assert!(
                s.segment_count.is_power_of_two(),
                "n={n} segment_count={}",
                s.segment_count
            );
        }
    }

    #[test]
    fn suggested_format_elem_capacity_minimum() {
        let s = suggested_format(1, 0);
        assert!(s.elem_capacity >= SUGGESTED_MIN_ELEM_CAPACITY);
        let s2 = suggested_format(0, 0);
        assert!(s2.elem_capacity >= SUGGESTED_MIN_ELEM_CAPACITY);
    }

    #[test]
    fn suggested_format_zero_vertices_uses_one_for_grid() {
        let s = suggested_format(0, 0);
        assert_eq!(s.segment_size, 1);
        assert_eq!(s.segment_count, 1);
    }
}
