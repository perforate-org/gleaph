//! Facade over canister-local vector index storage ([`store::VectorCanisterStore`]).

pub(crate) mod stable;

mod store;

pub use store::VectorCanisterStore;
pub(crate) use store::{advance_watermark, gc_subjects_step};

#[cfg(feature = "canbench")]
pub(crate) use store::SearchTuning;
