//! Facade over canister-local vector index storage ([`store::VectorCanisterStore`]).

pub(crate) mod stable;

mod store;

pub use store::VectorCanisterStore;

#[cfg(feature = "canbench")]
pub(crate) use store::SearchTuning;
