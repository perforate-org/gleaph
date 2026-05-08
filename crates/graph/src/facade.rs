//! Public coordination layer over stable storage.

mod store;

pub mod mutation_executor;

pub use store::{EdgeHandle, GraphStore, GraphStoreError};
