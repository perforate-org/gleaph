//! Bidirectional labeled LARA graph wrappers (deferred maintenance only).

pub(crate) mod deferred;
mod mate;
pub(crate) mod mate_blob_prototype;
pub(crate) mod mate_promotion;
pub(crate) mod mate_storage;

pub use deferred::{
    BidirectionalMaintenanceReport as LabeledBidirectionalMaintenanceReport,
    DeferredBidirectionalLabeledError, DeferredBidirectionalLabeledLaraGraph, DeleteEdgeObserver,
    EdgeSlotMoveObserver, MateStorageInitError, MateStorageMemories, Orientation, ScalarInsertPair,
};
pub use mate::{MateLookupError, PhysicalEdgeRef};
