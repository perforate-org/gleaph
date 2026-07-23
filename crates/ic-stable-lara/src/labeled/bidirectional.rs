//! Bidirectional labeled LARA graph wrappers (deferred maintenance only).

pub(crate) mod deferred;
mod mate;

pub use deferred::{
    BidirectionalMaintenanceReport as LabeledBidirectionalMaintenanceReport,
    DeferredBidirectionalLabeledError, DeferredBidirectionalLabeledLaraGraph, DeleteEdgeObserver,
    EdgeSlotMoveObserver, Orientation, ScalarInsertPair,
};
pub use mate::{MateLookupError, PhysicalEdgeRef};
