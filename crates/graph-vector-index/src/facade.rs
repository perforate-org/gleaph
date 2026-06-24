//! Facade over canister-local vector index storage ([`store::VectorIndexStore`]).

pub(crate) mod stable;

mod store;

pub use store::VectorIndexStore;

#[cfg(feature = "canbench")]
pub(crate) use store::SearchTuning;
