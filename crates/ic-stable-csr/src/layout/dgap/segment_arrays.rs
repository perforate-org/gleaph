//! Shared PMA mini-header constants and DGAP **vertex** → PMA leaf helpers.
//!
//! Per-node payload lives in [`super::segment_edge_counts`] (`SEC` region).

use crate::{SegmentId, VertexId};

pub const PMA_REGION_VERSION: u8 = 1;
/// Mini header: magic (3) + version (1) + reserved (12) — no duplicate of graph scalars.
pub const PMA_REGION_HEADER_SIZE: u64 = 16;

#[inline]
pub fn pma_node_id_for_vertex(vid: VertexId, segment_size: SegmentId, segment_count: SegmentId) -> usize {
    let leaf = u64::from(vid) / u64::from(segment_size);
    (leaf as usize).saturating_add(usize::from(segment_count))
}

#[inline]
pub fn dgap_leaf_segment_id(vid: VertexId, segment_size: SegmentId) -> SegmentId {
    SegmentId((u64::from(vid) / u64::from(segment_size)) as u32)
}
