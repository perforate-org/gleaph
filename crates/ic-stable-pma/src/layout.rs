//! On-disk layout helpers for `M_e` and `M_l`.
//!
//! All offsets are **per** underlying [`ic_stable_structures::Memory`] (address 0 is the start of that memory).
//! See crate root for how `M_v` / `M_e` / `M_l` map to separate memories.
//!
//! - [`dgap`] — byte layouts for the **three** DGAP `M_e` memories (PMA mini headers + `VCE` header + slab + logs).
//! - [`log_region`] — optional append-only stream journal (`M_l`), not used by [`crate::dgap::DgapEdgeStore`].

pub mod dgap;
pub mod log_region;

pub use dgap::{
    required_edges_and_log_bytes, required_segment_edges_actual_bytes,
    required_segment_edges_total_bytes, DgapEdgeHeaderV1, EDGE_REGION_MAGIC, EDGE_REGION_VERSION,
    EDGE_PAYLOAD_HEADER_SIZE, PMA_SEGMENT_EDGES_ACTUAL_MAGIC, PMA_SEGMENT_EDGES_TOTAL_MAGIC,
};
pub use log_region::{
    append_record, init_empty_log_region, LogRegionHeaderV1, LOG_HEADER_SIZE, LOG_REGION_MAGIC,
    LOG_REGION_VERSION,
};
