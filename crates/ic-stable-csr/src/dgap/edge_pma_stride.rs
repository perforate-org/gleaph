//! PMA `segment_edge_counts` node width (`stride`) from edge type capabilities (Rust `specialization`).
//!
//! [`crate::traits::CsrEdgeTombstone`] implementations reserve space for a per-node `tombstone: i64` counter.

use crate::traits::{CsrEdge, CsrEdgeTombstone};

/// Bytes per PMA tree node in the unified `segment_edge_counts` region: `16` (actual+total) or `24` (+ tombstone).
pub trait EdgePmaCountsStride {
    fn pma_counts_stride_bytes() -> u64;
}

impl<E: CsrEdge> EdgePmaCountsStride for E {
    default fn pma_counts_stride_bytes() -> u64 {
        16
    }
}

impl<E: CsrEdge + CsrEdgeTombstone> EdgePmaCountsStride for E {
    fn pma_counts_stride_bytes() -> u64 {
        24
    }
}
