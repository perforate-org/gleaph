//! On-disk layout helpers for `M_e` and `M_l`.

pub mod edge_region;
pub mod log_region;

pub use edge_region::{VcsrEdgeHeaderV1, EDGE_REGION_MAGIC, EDGE_REGION_VERSION};
pub use log_region::{LogRegionHeaderV1, LOG_REGION_MAGIC, LOG_REGION_VERSION};
