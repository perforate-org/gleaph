//! Optional **append-only stream journal** on a dedicated [`Memory`] (framed records in [`crate::layout::log_region`]).
//!
//! VCSR edge mutations **do not** use this module: overflow edges live in the `M_e` DGAP log pool
//! ([`crate::vcsr::VcsrEdgeStore`]). Use `dgap` here only when you need a separate replay / audit stream.
//!
//! # Relation to C++ DGAP (`reference/DGAP/dgap/src/graph.h`)
//!
//! The C++ reference uses per-segment `LogEntry` arrays **inside the edge region**; this crate mirrors that
//! layout in [`crate::layout::edge_region`] (v2 header + `log_segment_idx` + log pool). The stream format
//! below is **IC-specific** and is **not** byte-compatible with the reference’s on-disk OIDs.

pub use crate::layout::log_region::{
    append_record, init_empty_log_region, LogRegionHeaderV1, LOG_HEADER_SIZE, LOG_REGION_MAGIC,
    LOG_REGION_VERSION,
};
