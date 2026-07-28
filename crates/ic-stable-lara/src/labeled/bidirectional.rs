//! Bidirectional labeled LARA graph wrappers (deferred maintenance only).

pub mod counterpart;
pub(crate) mod deferred;

pub use counterpart::CanonicalEdgeOccurrence;
pub use deferred::{
    BidirectionalMaintenanceReport as LabeledBidirectionalMaintenanceReport,
    DeferredBidirectionalLabeledError, DeferredBidirectionalLabeledLaraGraph, DeleteEdgeObserver,
    DeletedEdge, EdgeSlotMoveObserver, Orientation, ScalarInsertPair,
};
