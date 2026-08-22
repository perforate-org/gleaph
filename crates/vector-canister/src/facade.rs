//! Facade over canister-local vector index storage: free functions over the stable-memory
//! statics owned by [`store`]'s sibling stable module.

pub(crate) mod stable;

pub(crate) mod store;

pub(crate) use store::VectorSyncBatchOutcomeOperationError;
pub(crate) use store::{advance_watermark, gc_subjects_step};

#[cfg(feature = "canbench")]
pub(crate) use store::SearchTuning;
