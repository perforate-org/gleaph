//! Shared PMA mini-header constants and DGAP **vertex** → PMA leaf helpers.
//!
//! Per-node payload lives in [`super::segment_edge_counts`] (`SEC` region).

pub const PMA_REGION_VERSION: u8 = 1;
/// Mini header: magic (3) + version (1) + reserved (12) — no duplicate of graph scalars.
pub const PMA_REGION_HEADER_SIZE: u64 = 16;

#[inline]
pub fn pma_node_id_for_vertex(vid: usize, segment_size: u32, segment_count: u32) -> usize {
    let leaf = (vid as u64) / (segment_size as u64);
    (leaf as usize).saturating_add(segment_count as usize)
}

#[inline]
pub fn dgap_leaf_segment_id(vid: usize, segment_size: u32) -> u32 {
    (vid as u64 / segment_size as u64) as u32
}
