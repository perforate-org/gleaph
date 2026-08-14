//! Facade over canister-local vector index storage ([`store::VectorCanisterStore`]).

pub(crate) mod stable;

mod store;

#[cfg(feature = "pocket-ic-e2e")]
pub(crate) use store::E2eSubjectPressureStep;
pub use store::VectorCanisterStore;
pub(crate) use store::VectorSyncBatchOutcomeOperationError;
pub(crate) use store::{advance_watermark, gc_subjects_step};

#[cfg(feature = "canbench")]
pub(crate) use store::SearchTuning;
