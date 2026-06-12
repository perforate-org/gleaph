//! Facade over canister-local index storage ([`store::IndexStore`]).

pub(crate) mod stable;

mod store;

pub use store::{DEFAULT_COUNT_POSTINGS_MAX_GROUPS, IndexStore};
