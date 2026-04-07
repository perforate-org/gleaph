//! On-disk DGAP edge layout split across **three** [`ic_stable_structures::Memory`] regions (`M_e`).
//!
//! Graph-wide scalars (`segment_count`, `elem_capacity`, …) live only in the **third** memory’s
//! [`DgapEdgeHeaderV1`]. The first two memories hold PMA segment-tree columns with their own V1 mini headers
//! ([`PMA_REGION_HEADER_SIZE`] each); see submodule docs for field-by-field diagrams.
//!
//! ```text
//! Memory 1 — `segment_edges_actual`
//! --------------------------------------------------
//! | `VCA` mini header + `i64` PMA tree (`segment_edges_actual` counts)              |
//! --------------------------------------------------
//!
//! Memory 2 — `segment_edges_total`
//! --------------------------------------------------
//! | `VCT` mini header + `i64` PMA tree (`segment_edges_total` / capacity)         |
//! --------------------------------------------------
//!
//! Memory 3 — `edges_and_log_segment`
//! --------------------------------------------------
//! | `VCE` [`DgapEdgeHeaderV1`] + CSR edge slab + `log_segment_idx[]` + log entry pool |
//! --------------------------------------------------
//! ```

mod edges_and_log;
mod header;
mod segment_arrays;
mod suggested_format;

pub use edges_and_log::{
    DGAP_DEFAULT_MAX_LOG_ENTRIES, EDGE_PAYLOAD_HEADER_SIZE, dgap_log_entry_stride,
    edge_slab_slot_offset, log_entry_offset, log_idx_base, log_pool_base, log_segment_idx_offset,
    read_log_segment_idx, required_edges_and_log_bytes, write_log_segment_idx,
};
pub use header::{DgapEdgeHeaderV1, EDGE_HEADER_SIZE, EDGE_REGION_MAGIC, EDGE_REGION_VERSION};
pub use segment_arrays::{
    PMA_REGION_HEADER_SIZE, PMA_REGION_VERSION, PMA_SEGMENT_EDGES_ACTUAL_MAGIC,
    PMA_SEGMENT_EDGES_TOTAL_MAGIC, dgap_leaf_segment_id, pma_array_bytes, pma_node_id_for_vertex,
    read_actual, read_total, required_segment_edges_actual_bytes,
    required_segment_edges_total_bytes, segment_array_data_offset, segment_edges_actual_region_ok,
    segment_edges_total_region_ok, write_actual, write_segment_edges_actual_region_header,
    write_segment_edges_total_region_header, write_total,
};
pub use suggested_format::{
    DgapSuggestedFormat, SUGGESTED_ELEM_CAPACITY_MULTIPLIER, SUGGESTED_MIN_ELEM_CAPACITY,
    suggested_format,
};
