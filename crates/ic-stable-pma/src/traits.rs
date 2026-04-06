//! Minimal traits for VCSR / CSR without Gleaph-specific types.
//!
//! `graph-pma` can implement [`CsrVertex`] for `VertexEntry` in a follow-up (avoids a dependency here).

use ic_stable_structures::Storable;

/// One vertex row in the CSR vertex column (`M_v`).
///
/// `log_head` is the DGAP per-segment log array index of the head of this vertex's overflow chain,
/// or `-1` if all neighbors live on the CSR slab (`gleaph-old/reference/DGAP/dgap/src/graph.h` `vertex_element.offset`).
pub trait CsrVertex: Storable + Copy {
    /// Global edge-slot index where this vertex's base neighborhood starts (flat slab model).
    fn base_slot_start(&self) -> u64;
    fn degree(&self) -> u32;
    fn with_base_slot_start(self, start: u64) -> Self;
    fn with_degree(self, degree: u32) -> Self;

    fn log_head(self) -> i32;
    fn with_log_head(self, idx: i32) -> Self;
}

/// One fixed-width edge slot in the CSR edge slab (`M_e`).
pub trait CsrEdgeSlot: Copy + Clone {
    const EDGE_BYTES: usize;
    fn read_from(bytes: &[u8]) -> Self;
    fn write_to(self, bytes: &mut [u8]);
}
