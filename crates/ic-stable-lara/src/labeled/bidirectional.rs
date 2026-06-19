//! Bidirectional labeled LARA graph wrappers (deferred maintenance only).

pub(crate) mod deferred;

pub use deferred::{
    BidirectionalMaintenanceReport as LabeledBidirectionalMaintenanceReport,
    DeferredBidirectionalLabeledError, DeferredBidirectionalLabeledLaraGraph, DeleteEdgeObserver,
    EdgeSlotMoveObserver, Orientation,
};
