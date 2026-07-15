//! Facade over canister-local index storage ([`store::IndexStore`]).

pub(crate) mod stable;

mod store;

pub use store::IndexStore;
