//! Multi-level labeled CSR variant of LARA.
//!
//! The labeled layout introduces an intermediate bucket CSR between vertices and
//! edges. Clean labeled scans read a vertex row, resolve one label bucket, then
//! walk a contiguous edge range without per-edge label filtering.

#[cfg(feature = "canbench")]
mod bench;
pub mod bidirectional;
pub mod deferred;
pub mod edge_slab;
pub mod graph;
pub mod invariants;
pub mod record;
pub mod row_store;
pub mod traits;

pub use bidirectional::{
    BidirectionalLabeledError, BidirectionalLabeledLaraGraph, DeferredBidirectionalLabeledLaraGraph,
};
pub use deferred::{DeferredError, DeferredLabeledLaraGraph, MaintenanceWorkItem};
pub use edge_slab::{
    EdgeSlabStore, HeaderV1 as LabeledEdgeHeaderV1, InitError as LabeledEdgeInitError,
};
pub use graph::{InitError as LabeledGraphInitError, LabeledLaraGraph, LabeledOperationError};
pub use record::{LabelBucket, LabelId, LabeledVertex};
pub use row_store::{InitError as LabeledRowInitError, RowStore};
pub use traits::LabeledCsrVertex;

/// Convenience alias for the single-orientation labeled LARA graph.
pub type LabeledLara<E, M> = LabeledLaraGraph<E, M>;
/// Convenience alias for the deferred-maintenance labeled LARA graph.
pub type DeferredLabeledLara<E, M> = DeferredLabeledLaraGraph<E, M>;
/// Convenience alias for the bidirectional labeled LARA graph.
pub type BidirectionalLabeledLara<E, M> = BidirectionalLabeledLaraGraph<E, M>;
/// Convenience alias for the deferred bidirectional labeled LARA graph.
pub type DeferredBidirectionalLabeledLara<E, M> = DeferredBidirectionalLabeledLaraGraph<E, M>;
