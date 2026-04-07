//! On-disk DGAP edge layout split across **two** [`ic_stable_structures::Memory`] regions (`M_e`).
//!
//! Graph-wide scalars (`segment_count`, `elem_capacity`, …) live only in the **second** memory’s
//! [`DgapEdgeHeaderV1`]. The first memory holds the unified PMA `segment_edge_counts` tree ([`PMA_SEGMENT_EDGE_COUNTS_MAGIC`]).
//!
//! ```text
//! Memory 1 — `segment_edge_counts`
//! --------------------------------------------------
//! | `SEC` mini header + packed [`SegmentEdgeCounts`] per PMA node (stride 16 or 24)   |
//! --------------------------------------------------
//!
//! Memory 2 — `edges_and_log_segment`
//! --------------------------------------------------
//! | `VCE` [`DgapEdgeHeaderV1`] + CSR slab + `log_segment_idx[]` + log entry pool     |
//! --------------------------------------------------
//! ```

mod edges_and_log;
mod header;
mod segment_arrays;
mod segment_edge_counts;
mod suggested_format;

pub use edges_and_log::{
    DGAP_DEFAULT_MAX_LOG_ENTRIES, EDGE_PAYLOAD_HEADER_SIZE, dgap_log_entry_stride,
    edge_slab_slot_offset, log_entry_offset, log_idx_base, log_pool_base, log_segment_idx_offset,
    read_log_segment_idx, required_edges_and_log_bytes, write_log_segment_idx,
};
pub use header::{DgapEdgeHeaderV1, EDGE_HEADER_SIZE, EDGE_REGION_MAGIC, EDGE_REGION_VERSION};
pub use segment_arrays::{
    PMA_REGION_HEADER_SIZE, PMA_REGION_VERSION, dgap_leaf_segment_id, pma_node_id_for_vertex,
};
pub use segment_edge_counts::{
    PMA_SEGMENT_EDGE_COUNTS_MAGIC, SegmentEdgeCounts, pma_edge_counts_array_bytes,
    read_segment_edge_counts, required_segment_edge_counts_bytes, segment_edge_counts_node_offset,
    segment_edge_counts_region_ok, write_segment_edge_counts, write_segment_edge_counts_region_header,
};
pub use suggested_format::{
    DgapSuggestedFormat, SUGGESTED_ELEM_CAPACITY_MULTIPLIER, SUGGESTED_MIN_ELEM_CAPACITY,
    suggested_format,
};
