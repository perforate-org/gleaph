//! Facade over canister-local vector index storage ([`store::VectorIndexStore`]).

pub(crate) mod stable;

mod store;

pub use store::VectorIndexStore;
